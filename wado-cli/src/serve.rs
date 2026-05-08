use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::pin::pin;
use std::process;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use futures::future::{Either, select};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Collected, Full};
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use lexopt::Arg::Value;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use wasmtime::Store;
use wasmtime::component::{Component, Linker};
use wasmtime::Engine;
use wasmtime_wasi_http::p3::Request as WasiRequest;
use wasmtime_wasi_http::p3::bindings::Service;

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, OptLevel};
use crate::manifest;
use crate::runtime::{self, WasiState};
use wado_compiler::LogLevel;

pub struct ServeOptions {
    pub input: String,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub addr: String,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    pub allocator: Option<String>,
    /// Preopened directories as `(host_path, guest_path)` pairs. Empty by
    /// default — services rarely need filesystem access, so unlike `wado run`
    /// we do NOT preopen the cwd unless the user passes `--dir`.
    pub preopened_dirs: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
enum Opt {
    Addr,
    Dir,
    OptLevel,
    InlineThreshold,
    OptIterations,
    LogLevel,
    Allocator,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Addr,
        Self::Dir,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::LogLevel,
        Self::Allocator,
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
            // `serve` exposes `--dir` but not `--no-dir`: the default for
            // a service is "no preopens" (services rarely need filesystem
            // access), so `--no-dir` would have nothing to disable. We
            // intentionally diverge from `run`/`test` here rather than
            // accept a misleading no-op flag.
            Self::Dir => args::DIR_SPEC,
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::Allocator => args::ALLOCATOR_SPEC,
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
    let mut allocator: Option<String> = None;
    let mut preopened_dirs: Vec<(String, String)> = Vec::new();

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Addr => addr = args::require_string(&mut parser)?,
                Opt::Dir => preopened_dirs.push(args::parse_dir_arg(&mut parser)?),
                Opt::OptLevel => opt_level = compile::parse_opt_level_arg(&mut parser)?,
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
                Opt::Allocator => allocator = Some(args::require_string(&mut parser)?),
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
        allocator,
        preopened_dirs,
    })
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
///
/// The success body is returned as a `Collected<Bytes>` (boxed) rather than
/// `Full<Bytes>` so that response trailers written by the guest survive the
/// hop into hyper. `Full<Bytes>` carries no trailer frames, which would silently
/// strip e.g. `Server-Timing` written via `trailers_tx.write(Some(...))`.
async fn handle_http_request(
    engine: &Engine,
    component: &Component,
    linker: &Linker<WasiState>,
    preopened_dirs: &[(String, String)],
    req: HyperRequest<hyper::body::Incoming>,
) -> Result<HyperResponse<BoxBody<Bytes, Infallible>>> {
    type HttpErrorCode = wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

    // Each request gets a fresh WASI state — preopened dirs are scoped
    // per-request. Use the no-env variant: an HTTP server's environment
    // typically holds secrets (DB creds, API tokens) that must not leak
    // into per-request handler components, matching the pre-refactor
    // behaviour where serve only inherited stdio.
    let state = WasiState::new_no_inherit_env(preopened_dirs, &[])?;
    let mut store = Store::new(engine, state);

    let service = Service::instantiate_async(&mut store, component, linker).await?;

    let (parts, body) = req.into_parts();
    let body = body.map_err(HttpErrorCode::from_hyper_request_error);
    let http_req = http::Request::from_parts(parts, body);
    let (wasi_req, io) = WasiRequest::from_http(http_req);

    let result =
        store
            .run_concurrent(
                async |store| -> Result<
                    Result<http::Response<Collected<Bytes>>, Option<HttpErrorCode>>,
                > {
                    let handler = pin!(async {
                        let res = match service.handle(store, wasi_req).await? {
                            Ok(res) => res,
                            Err(err) => return anyhow::Ok(Err(Some(err))),
                        };
                        let res = store.with(|store| res.into_http(store, async { Ok(()) }))?;
                        let (parts, body) = res.into_parts();
                        let collected = BodyExt::collect(body)
                            .await
                            .map_err(|e| anyhow::anyhow!("failed to collect response body: {e}"))?;
                        anyhow::Ok(Ok(http::Response::from_parts(parts, collected)))
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
                .body(error_body(format!("Handler trapped:\n{e:?}")))?);
        }
        Err(e) => {
            eprintln!("Handler trapped: {e:?}");
            return Ok(HyperResponse::builder()
                .status(500)
                .body(error_body(format!("Handler trapped:\n{e:?}")))?);
        }
    };

    match result {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            Ok(HyperResponse::from_parts(parts, BoxBody::new(body)))
        }
        Err(Some(error_code)) => Ok(HyperResponse::builder()
            .status(500)
            .body(error_body(format!("{error_code:?}")))?),
        Err(None) => Ok(HyperResponse::builder()
            .status(500)
            .body(error_body("Handler returned error".to_string()))?),
    }
}

fn error_body(msg: String) -> BoxBody<Bytes, Infallible> {
    BoxBody::new(Full::new(Bytes::from(msg)))
}

async fn run_http_server(
    wasm: Vec<u8>,
    addr: &str,
    cranelift_opt: wasmtime::OptLevel,
    preopened_dirs: Vec<(String, String)>,
) -> Result<()> {
    let engine = runtime::create_engine(cranelift_opt, &runtime::ProfileMode::None)?;
    let component = Component::new(&engine, &wasm)?;
    let linker = runtime::create_linker(&engine)?;

    // Wrap in Arc for sharing across connections
    let engine = Arc::new(engine);
    let component = Arc::new(component);
    let linker = Arc::new(Mutex::new(linker));
    let preopened_dirs = Arc::new(preopened_dirs);

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
        let preopened_dirs = Arc::clone(&preopened_dirs);

        tokio::spawn(async move {
            let service = service_fn(|req| {
                let engine = Arc::clone(&engine);
                let component = Arc::clone(&component);
                let linker = Arc::clone(&linker);
                let preopened_dirs = Arc::clone(&preopened_dirs);

                async move {
                    let linker = linker.lock().await;
                    handle_http_request(&engine, &component, &linker, &preopened_dirs, req).await
                }
            });

            // Auto-detect HTTP/1.1 vs h2c (HTTP/2 cleartext, prior-knowledge)
            // by sniffing the connection preface. HTTP/1 callers see normal
            // behavior; h2c clients (e.g. `curl --http2-prior-knowledge`) get
            // trailers without needing the `TE: trailers` handshake hyper
            // requires on the HTTP/1 path.
            let builder = auto::Builder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection(io, service).await {
                eprintln!("Error serving {remote_addr}: {e}");
            }
        });
    }
}

pub async fn run(opts: ServeOptions) {
    let flags = CompileFlags {
        opt_level: opts.opt_level,
        log_level: opts.log_level,
        target_world: Some("wasi:http/service".to_string()),
        skip_validation: false,
        inline_threshold: opts.inline_threshold,
        opt_iterations: opts.opt_iterations,
        allocator: opts.allocator,
    };
    let cranelift_opt = opts.opt_level.to_wasmtime();
    let wasm = compile::compile(&opts.input, &flags).await;

    if let Err(e) = run_http_server(wasm, &opts.addr, cranelift_opt, opts.preopened_dirs).await {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
