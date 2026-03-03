use std::fmt::Write as _;
use std::net::SocketAddr;
use std::process;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use futures::try_join;
use http_body_util::{BodyExt, Empty, Full};
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
use wasmtime_wasi_http::p3::bindings::Service;
use wasmtime_wasi_http::p3::{Request as WasiRequest, WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::args::{self, CliExit};
use crate::compile::{self, OptLevel};
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
#[allow(clippy::enum_variant_names)]
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
        input: args::require_input(input, &usage)?,
        opt_level,
        log_level,
        addr,
        inline_threshold,
        opt_iterations,
    })
}

struct HttpWasiCtx;

impl WasiHttpCtx for HttpWasiCtx {}

struct HttpWasiState {
    table: ResourceTable,
    wasi: WasiCtx,
    http: HttpWasiCtx,
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
        http: HttpWasiCtx,
    }
}

/// Handle a single HTTP request using the Wasm component
async fn handle_http_request(
    engine: &Engine,
    component: &Component,
    linker: &Linker<HttpWasiState>,
    req: HyperRequest<hyper::body::Incoming>,
) -> Result<HyperResponse<Full<Bytes>>> {
    let state = create_http_state();
    let mut store = Store::new(engine, state);

    let service = Service::instantiate_async(&mut store, component, linker).await?;

    // Convert hyper request to http::Request with Empty body
    let (parts, _body) = req.into_parts();
    let http_req = http::Request::from_parts(parts, Empty::<Bytes>::new());

    let (wasi_req, io) = WasiRequest::from_http(http_req);

    // Channel to receive the response
    let (tx, rx) = tokio::sync::oneshot::channel();

    // Run handler and response receiver in parallel
    let result = try_join!(
        async {
            store
                .run_concurrent(async |store| {
                    let (res, task) = match service.handle(store, wasi_req).await? {
                        Ok(pair) => pair,
                        Err(err) => return Ok(Err(Some(err))),
                    };
                    let _ = tx.send(store.with(|store| res.into_http(store, async { Ok(()) }))?);
                    task.block(store).await;
                    Ok(Ok(()))
                })
                .await?
        },
        async {
            let res = rx.await?;
            let (parts, body) = res.into_parts();
            let body = body.collect().await?;
            anyhow::Ok(http::Response::from_parts(parts, body))
        }
    );

    // Drop io - we don't consume request body in simple cases
    drop(io);

    match result {
        Ok((Ok(()), res)) => {
            let (parts, body) = res.into_parts();
            Ok(HyperResponse::from_parts(parts, Full::new(body.to_bytes())))
        }
        Ok((Err(Some(error_code)), _)) => {
            // Handler returned error code - map to HTTP 500
            Ok(HyperResponse::builder()
                .status(500)
                .body(Full::new(Bytes::from(format!("{error_code:?}"))))?)
        }
        Ok((Err(None), _)) => Ok(HyperResponse::builder()
            .status(500)
            .body(Full::new(Bytes::from("Handler returned error")))?),
        Err(e) => Ok(HyperResponse::builder()
            .status(500)
            .body(Full::new(Bytes::from(format!("Internal error: {e}"))))?),
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
    )
    .await;

    if let Err(e) = run_http_server(wasm, &opts.addr).await {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
