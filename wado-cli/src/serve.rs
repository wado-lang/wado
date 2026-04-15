use std::fmt::Write as _;
use std::net::SocketAddr;
use std::pin::pin;
use std::process;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use futures::future::{Either, select};
use http_body_util::{BodyExt, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::TokioIo;
use lexopt::Arg::Value;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::WasiHttpCtx;
use wasmtime_wasi_http::p3::bindings::Service;
use wasmtime_wasi_http::p3::{Request as WasiRequest, WasiHttpCtxView, WasiHttpView};

use crate::args::{self, CliExit};
use crate::compile::{self, OptLevel};
use crate::manifest;
use crate::runtime;
use wado_compiler::LogLevel;

pub struct ServeOptions {
    pub input: String,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub addr: String,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
}

#[derive(Clone, Copy)]
enum Opt {
    Addr,
    OptLevel,
    InlineThreshold,
    OptIterations,
    LogLevel,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Addr,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::LogLevel,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Addr => args::OptSpec {
                long: Some("addr"),
                short: None,
                value: Some("<addr>"),
                desc: "Address to listen on (default: 0.0.0.0:8080)",
            },
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado serve [options] <file.wado>").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Compile and serve a Wado HTTP service using wasmtime.").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse command-line arguments for the `serve` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<ServeOptions, CliExit> {
    let usage = format_usage();
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut log_level = LogLevel::default();
    let mut addr = "0.0.0.0:8080".to_string();
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Addr => addr = args::require_string(&mut parser)?,
                Opt::OptLevel => {
                    let val = parser.optional_value();
                    let level_str = val
                        .as_ref()
                        .map(|v| v.to_string_lossy())
                        .unwrap_or_default();
                    opt_level = match level_str.as_ref() {
                        "" | "0" | "g" => OptLevel::O0,
                        "1" => OptLevel::O1,
                        "2" => OptLevel::O2,
                        "3" => OptLevel::O3,
                        "s" => OptLevel::Os,
                        _ => {
                            return Err(CliExit::error(format!(
                                "unknown optimization level '-O{level_str}'. Use -O0, -O1, -O2, -O3, -Os, or -Og"
                            )));
                        }
                    };
                }
                Opt::InlineThreshold => {
                    inline_threshold = Some(args::parse_inline_threshold_arg(
                        "--optimize-inline-threshold",
                        &mut parser,
                    )?);
                }
                Opt::OptIterations => {
                    opt_iterations = Some(args::parse_opt_iterations_arg(
                        "--optimize-iterations",
                        &mut parser,
                    )?);
                }
                Opt::LogLevel => log_level = args::parse_log_level_arg(&mut parser)?,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            args::reject_multiple_inputs(&input)?;
            input = Some(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    Ok(ServeOptions {
        input: manifest::resolve_input(input, manifest::EntryPointKind::Service, &usage)?,
        opt_level,
        log_level,
        addr,
        inline_threshold,
        opt_iterations,
    })
}

struct HttpWasiState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
}

impl WasiView for HttpWasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for HttpWasiState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

fn create_http_linker(engine: &Engine) -> Result<Linker<HttpWasiState>> {
    let mut linker: Linker<HttpWasiState> = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    Ok(linker)
}

fn create_http_state() -> HttpWasiState {
    HttpWasiState {
        table: ResourceTable::new(),
        wasi: WasiCtxBuilder::new().inherit_stdio().build(),
        http: WasiHttpCtx::new(),
    }
}

/// Handle a single HTTP request using the Wasm component.
///
/// Mirrors the wasmtime p3 reference and wado-compiler e2e harness pattern:
/// both the guest handler and the response-body collection run inside
/// `run_concurrent`, so the guest's post-`task-return` continuation keeps
/// getting polled until it finishes writing the body and trailers. Collecting
/// outside `run_concurrent` deadlocks because the store stops polling as soon
/// as the closure future resolves.
///
/// The handler arm and the request-body I/O arm are raced via `futures::future::select`
/// — handler completing first is the normal case (a guest may never consume
/// the request body, in which case the I/O future only resolves when the
/// request resource is dropped).
async fn handle_http_request(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HttpWasiState>,
    req: HyperRequest<hyper::body::Incoming>,
) -> Result<HyperResponse<Full<Bytes>>> {
    type HttpErrorCode = wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

    let state = create_http_state();
    let mut store = Store::new(engine, state);

    let service = Service::instantiate_async(&mut store, component, linker).await?;

    let (parts, body) = req.into_parts();
    let body = body.map_err(HttpErrorCode::from_hyper_request_error);
    let http_req = http::Request::from_parts(parts, body);
    let (wasi_req, io) = WasiRequest::from_http(http_req);

    let result = store
        .run_concurrent(
            async |store| -> Result<Result<http::Response<Bytes>, Option<HttpErrorCode>>> {
                let handler = pin!(async {
                    let res = match service.handle(store, wasi_req).await? {
                        Ok(res) => res,
                        Err(err) => return anyhow::Ok(Err(Some(err))),
                    };
                    let res = store.with(|store| res.into_http(store, async { Ok(()) }))?;
                    let (parts, body) = res.into_parts();
                    let body = BodyExt::collect(body)
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to collect response body: {e}"))?
                        .to_bytes();
                    anyhow::Ok(Ok(http::Response::from_parts(parts, body)))
                });
                let io = pin!(async {
                    io.await
                        .map_err(|e| anyhow::anyhow!("request body I/O: {e}"))
                });
                match select(handler, io).await {
                    Either::Left((result, _)) => result,
                    Either::Right((result, _)) => result.map(|()| Err(None)),
                }
            },
        )
        .await;

    let result = match result {
        Ok(Ok(inner)) => inner,
        Ok(Err(e)) => {
            eprintln!("Handler trapped: {e:?}");
            return Ok(HyperResponse::builder()
                .status(500)
                .body(Full::new(Bytes::from(format!("Handler trapped:\n{e:?}"))))?);
        }
        Err(e) => {
            eprintln!("Handler trapped: {e:?}");
            return Ok(HyperResponse::builder()
                .status(500)
                .body(Full::new(Bytes::from(format!("Handler trapped:\n{e:?}"))))?);
        }
    };

    match result {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            Ok(HyperResponse::from_parts(parts, Full::new(body)))
        }
        Err(Some(error_code)) => Ok(HyperResponse::builder()
            .status(500)
            .body(Full::new(Bytes::from(format!("{error_code:?}"))))?),
        Err(None) => Ok(HyperResponse::builder()
            .status(500)
            .body(Full::new(Bytes::from("Handler returned error")))?),
    }
}

async fn run_http_server(wasm: Vec<u8>, addr: &str) -> Result<()> {
    let engine = runtime::create_engine(wasmtime::OptLevel::Speed, &runtime::ProfileMode::None)?;
    let component = Component::new(&engine, &wasm)?;
    let linker = create_http_linker(&engine)?;

    // Wrap in Arc for sharing across connections
    let engine = Arc::new(engine);
    let component = Arc::new(component);
    let linker = Arc::new(Mutex::new(linker));

    let addr: SocketAddr = addr.parse()?;
    let listener = TcpListener::bind(addr).await?;

    eprintln!("HTTP server listening on http://{addr}/");
    eprintln!("Press Ctrl+C to stop");

    loop {
        let (stream, remote_addr) = listener.accept().await?;
        let io = TokioIo::new(stream);

        let engine = Arc::clone(&engine);
        let component = Arc::clone(&component);
        let linker = Arc::clone(&linker);

        tokio::spawn(async move {
            let service = service_fn(|req| {
                let engine = Arc::clone(&engine);
                let component = Arc::clone(&component);
                let linker = Arc::clone(&linker);

                async move {
                    let linker = linker.lock().await;
                    handle_http_request(&engine, &component, &linker, req).await
                }
            });

            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("Error serving {remote_addr}: {e}");
            }
        });
    }
}

pub async fn run(opts: ServeOptions) {
    let wasm = compile::compile_with_full_opts(
        &opts.input,
        opts.opt_level,
        opts.log_level,
        Some("wasi:http/service".to_string()),
        false,
        opts.inline_threshold,
        opts.opt_iterations,
        None,
    )
    .await;

    if let Err(e) = run_http_server(wasm, &opts.addr).await {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
