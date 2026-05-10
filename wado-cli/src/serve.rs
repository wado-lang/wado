use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::pin::pin;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
use tokio::task::JoinSet;
use wasmtime::Engine;
use wasmtime::Store;
use wasmtime::component::Component;
use wasmtime_wasi_http::p3::Request as WasiRequest;
use wasmtime_wasi_http::p3::bindings::{Service, ServicePre};

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, OptLevel};
use crate::manifest;
use crate::runtime::{self, Preopens, WasiState};
use wado_compiler::LogLevel;

/// Default per-request timeout in seconds. A guest that fails to produce a
/// response within this window has its epoch deadline expired (via
/// `Store::set_epoch_deadline`), which causes a `Trap::Interrupt`, freeing
/// the tokio task and letting the client see a 504.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Epoch ticker interval. Each tick is a unit of `set_epoch_deadline`, so
/// a 1s tick + a deadline of `timeout_secs` ticks yields second-granularity
/// timeout enforcement — sufficient for the 30s default.
const EPOCH_TICK_MS: u64 = 1000;

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
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
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
    Timeout,
    Help,
}

const TIMEOUT_SPEC: args::OptSpec = args::OptSpec {
    long: Some("timeout"),
    short: None,
    value: Some("<seconds>"),
    desc: "Per-request timeout in seconds (default: 30)",
};

impl Opt {
    const ALL: &[Self] = &[
        Self::Addr,
        Self::Dir,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::LogLevel,
        Self::Allocator,
        Self::Timeout,
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
            Self::Timeout => TIMEOUT_SPEC,
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

fn parse_timeout_arg(parser: &mut lexopt::Parser) -> Result<u64, CliExit> {
    let s = args::require_string(parser)?;
    let n = s.parse::<u64>().map_err(|_| {
        CliExit::error(format!(
            "--timeout requires a positive integer (seconds), got '{s}'"
        ))
    })?;
    if n == 0 {
        return Err(CliExit::error("--timeout must be > 0"));
    }
    Ok(n)
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
    let mut timeout_secs: u64 = DEFAULT_TIMEOUT_SECS;

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
                Opt::Timeout => timeout_secs = parse_timeout_arg(&mut parser)?,
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
        timeout_secs,
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
///
/// Per-request timeout is enforced two ways: `set_epoch_deadline` traps the
/// guest if it stays inside wasm past the deadline (handles tight CPU loops
/// that never await), and `tokio::time::timeout` cancels the future at the
/// async boundary (handles host I/O hangs). The combination ensures a hung
/// request never holds a tokio task longer than `timeout`.
async fn handle_http_request(
    engine: &Engine,
    service_pre: &ServicePre<WasiState>,
    preopens: &Preopens,
    timeout: Duration,
    req: HyperRequest<hyper::body::Incoming>,
) -> Result<HyperResponse<BoxBody<Bytes, Infallible>>> {
    type HttpErrorCode = wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;

    // Each request gets a fresh WASI state — the WASI ctx is scoped
    // per-request — but the preopened dirs are opened once at server
    // startup and shared via reference-counted `cap_std::fs::Dir` handles,
    // so per-request setup does not re-`openat` the host paths. Use the
    // no-env variant: an HTTP server's environment typically holds secrets
    // (DB creds, API tokens) that must not leak into per-request handler
    // components, matching the pre-refactor behaviour where serve only
    // inherited stdio.
    let state = WasiState::new_no_inherit_env_with_preopens(preopens, &[]);
    let mut store = Store::new(engine, state);
    // Set the epoch deadline so a guest stuck in pure wasm traps after
    // `timeout` ticks (the engine is incremented every `EPOCH_TICK_MS`).
    let deadline_ticks = timeout.as_secs().max(1);
    store.set_epoch_deadline(deadline_ticks);

    let timeout_result = tokio::time::timeout(timeout, async {
        let service: Service = service_pre.instantiate_async(&mut store).await?;

        let (parts, body) = req.into_parts();
        let body = body.map_err(HttpErrorCode::from_hyper_request_error);
        let http_req = http::Request::from_parts(parts, body);
        let (wasi_req, io) = WasiRequest::from_http(http_req);

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
            .await
    })
    .await;

    let result = match timeout_result {
        Ok(Ok(Ok(inner))) => inner,
        Ok(Ok(Err(e))) => {
            eprintln!("Handler trapped: {e:?}");
            return Ok(HyperResponse::builder()
                .status(500)
                .body(error_body(format!("Handler trapped:\n{e:?}")))?);
        }
        Ok(Err(e)) => {
            // run_concurrent returned Err — most commonly an epoch trap from
            // a guest that exceeded the deadline while inside wasm.
            if is_epoch_deadline_error(&e) {
                eprintln!("Handler timed out after {}s", timeout.as_secs());
                return Ok(HyperResponse::builder()
                    .status(504)
                    .body(error_body(format!(
                        "Handler timed out after {}s",
                        timeout.as_secs()
                    )))?);
            }
            eprintln!("Handler trapped: {e:?}");
            return Ok(HyperResponse::builder()
                .status(500)
                .body(error_body(format!("Handler trapped:\n{e:?}")))?);
        }
        Err(_elapsed) => {
            // Future-level timeout fired (host I/O hang or yield path).
            eprintln!("Handler timed out after {}s", timeout.as_secs());
            return Ok(HyperResponse::builder()
                .status(504)
                .body(error_body(format!(
                    "Handler timed out after {}s",
                    timeout.as_secs()
                )))?);
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

fn is_epoch_deadline_error(err: &wasmtime::Error) -> bool {
    err.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
}

fn error_body(msg: String) -> BoxBody<Bytes, Infallible> {
    BoxBody::new(Full::new(Bytes::from(msg)))
}

/// Wait for a shutdown signal (SIGINT or, on Unix, SIGTERM). Resolves on
/// the first signal received.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: failed to install SIGTERM handler: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        let ctrl_c = pin!(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        let term = pin!(async {
            let _ = sigterm.recv().await;
        });
        let _ = select(ctrl_c, term).await;
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn run_http_server(
    wasm: Vec<u8>,
    addr: &str,
    cranelift_opt: wasmtime::OptLevel,
    preopened_dirs: Vec<(String, String)>,
    timeout: Duration,
) -> Result<()> {
    let engine = runtime::create_serve_engine(cranelift_opt)?;
    let component = Component::new(&engine, &wasm)?;
    let linker = runtime::create_linker(&engine)?;
    // Open preopens once here, then share across requests. Each request
    // attaches them via `WasiState::new_no_inherit_env_with_preopens`,
    // which clones the underlying `Arc<cap_std::fs::Dir>` rather than
    // re-running `openat` per request.
    let preopens = Preopens::open(&preopened_dirs)?;
    // Pre-link the component once. This front-loads the per-export string
    // lookups that `Service::instantiate_async` would otherwise repeat on
    // every request.
    let instance_pre = linker.instantiate_pre(&component)?;
    let service_pre = ServicePre::<WasiState>::new(instance_pre)?;

    // Drop the linker — the `InstancePre` already captured everything we
    // need, and keeping the linker around would force every request to
    // share it (the linker is `&self`, but holding it serves no purpose).
    drop(linker);
    drop(component);

    let engine = Arc::new(engine);
    let service_pre = Arc::new(service_pre);
    let preopens = Arc::new(preopens);

    // Background ticker drives `Engine::increment_epoch` so the per-store
    // `set_epoch_deadline` actually fires. Stopped via `epoch_stop` once
    // the accept loop exits.
    let epoch_stop = Arc::new(AtomicBool::new(false));
    let epoch_thread = {
        let stop = Arc::clone(&epoch_stop);
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
                engine.increment_epoch();
            }
        })
    };

    let addr: SocketAddr = addr.parse()?;
    let listener = TcpListener::bind(addr).await?;

    eprintln!("HTTP server listening on http://{addr}/");
    eprintln!("Per-request timeout: {}s", timeout.as_secs());
    #[cfg(unix)]
    eprintln!("Send SIGINT or SIGTERM to shut down");
    #[cfg(not(unix))]
    eprintln!("Send Ctrl+C to shut down");

    let mut connections: JoinSet<()> = JoinSet::new();
    let mut shutdown = pin!(shutdown_signal());

    // Accept loop with graceful shutdown. On signal, stop accepting and
    // wait for in-flight connections to drain (with a hard cap so we don't
    // hang forever on misbehaving clients).
    loop {
        let accept = pin!(listener.accept());
        match select(accept, shutdown.as_mut()).await {
            Either::Right(((), _accept)) => {
                eprintln!(
                    "Shutdown signal received; draining {} in-flight connection(s)…",
                    connections.len()
                );
                break;
            }
            Either::Left((Err(e), _shutdown)) => {
                eprintln!("accept error: {e}");
            }
            Either::Left((Ok((stream, remote_addr)), _shutdown)) => {
                let io = TokioIo::new(stream);

                let engine = Arc::clone(&engine);
                let service_pre = Arc::clone(&service_pre);
                let preopens = Arc::clone(&preopens);

                connections.spawn(async move {
                    let svc = service_fn(|req| {
                        let engine = Arc::clone(&engine);
                        let service_pre = Arc::clone(&service_pre);
                        let preopens = Arc::clone(&preopens);

                        async move {
                            handle_http_request(&engine, &service_pre, &preopens, timeout, req)
                                .await
                        }
                    });

                    // Auto-detect HTTP/1.1 vs h2c (HTTP/2 cleartext, prior-knowledge)
                    // by sniffing the connection preface. HTTP/1 callers see normal
                    // behavior; h2c clients (e.g. `curl --http2-prior-knowledge`) get
                    // trailers without needing the `TE: trailers` handshake hyper
                    // requires on the HTTP/1 path.
                    let builder = auto::Builder::new(TokioExecutor::new());
                    if let Err(e) = builder.serve_connection(io, svc).await {
                        eprintln!("Error serving {remote_addr}: {e}");
                    }
                });

                // Non-blockingly reap any connections that finished while
                // we were accepting, so the JoinSet doesn't grow unboundedly
                // on a long-running server.
                while connections.try_join_next().is_some() {}
            }
        }
    }

    // Drain phase: stop accepting, give in-flight connections a bounded
    // window to finish. Anything still alive at the deadline gets aborted.
    let drain_deadline = timeout + Duration::from_secs(5);
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(drain_deadline, drain).await.is_err() {
        eprintln!(
            "Drain timeout after {}s; aborting remaining connections",
            drain_deadline.as_secs()
        );
        connections.shutdown().await;
    }

    epoch_stop.store(true, Ordering::Relaxed);
    // The ticker sleeps `EPOCH_TICK_MS` between checks, so the join wakes
    // up within that window.
    let _ = epoch_thread.join();

    Ok(())
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

    let timeout = Duration::from_secs(opts.timeout_secs);
    if let Err(e) = run_http_server(
        wasm,
        &opts.addr,
        cranelift_opt,
        opts.preopened_dirs,
        timeout,
    )
    .await
    {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
