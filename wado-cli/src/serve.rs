use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use futures::future::{Either, poll_fn, select};
use http_body::{Body as _, Frame};
use http_body_util::BodyExt as _;
use http_body_util::Full;
use http_body_util::combinators::UnsyncBoxBody;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use lexopt::Arg::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
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

/// Outcome the spawned handler task delivers to the request entry point.
///
/// `Streaming` is the success path: response head plus a `Receiver` that
/// will yield each body frame as the guest produces it. The other variants
/// are terminal failure modes that have to be rendered as a synthetic 5xx
/// because no part of the response has been put on the wire yet.
enum HandlerOutcome {
    Streaming(http::response::Parts, mpsc::Receiver<Frame<Bytes>>),
    GuestError(wasmtime_wasi_http::p3::bindings::http::types::ErrorCode),
    Trapped(String),
    Timeout,
}

/// `http_body::Body` implementation backed by a tokio `mpsc::Receiver` of
/// frames. Used to stream the guest's response body to hyper one frame at
/// a time without buffering the full body in memory.
///
/// Trailers travel through the same channel as `Frame::trailers(...)`.
struct ChannelBody {
    rx: mpsc::Receiver<Frame<Bytes>>,
}

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

type StreamingBody = UnsyncBoxBody<Bytes, Infallible>;

fn error_response(status: u16, msg: String) -> HyperResponse<StreamingBody> {
    let body: Full<Bytes> = Full::new(Bytes::from(msg));
    HyperResponse::builder()
        .status(status)
        .body(UnsyncBoxBody::new(body))
        .expect("static status code should always build successfully")
}

/// Handle a single HTTP request using the Wasm component, **streaming** the
/// response body frame-by-frame.
///
/// The store is moved into a spawned task that drives `run_concurrent` for
/// the entire lifetime of the response, including the guest's post-`task
/// return` continuation. As soon as the head is available the spawn task
/// hands the response head plus a body channel back here via a oneshot;
/// from then on hyper drains body frames directly from that channel while
/// the spawn task keeps producing them. Because the channel is bounded
/// (capacity 8), a slow consumer naturally back-pressures the guest.
///
/// The per-request timeout is enforced in two layered ways:
///
/// * `Store::set_epoch_deadline` plus the engine-wide ticker traps the
///   guest if it stays inside wasm past the deadline. This bounds the
///   total request lifetime — even a long-lived streaming response is
///   cut off when the deadline expires, since the wasm side traps and
///   the spawn task drops the body sender on the way out.
/// * A first-byte `tokio::time::sleep(timeout)` raced against the head
///   oneshot returns 504 to the client if the guest never produces a
///   response head — covering the case where the guest is stuck waiting
///   on host I/O instead of running wasm code.
///
/// Hang avoidance:
///
/// * Hyper dropping the body causes `frame_tx.send` to fail, breaking the
///   pump loop; the spawn task then exits.
/// * If the outer first-byte timeout fires, the spawn task is left to run,
///   but the per-store epoch deadline guarantees it terminates within one
///   tick interval of the configured timeout.
async fn handle_http_request(
    engine: &Engine,
    service_pre: &ServicePre<WasiState>,
    preopens: &Preopens,
    timeout: Duration,
    req: HyperRequest<hyper::body::Incoming>,
) -> Result<HyperResponse<StreamingBody>> {
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
    let timeout_secs = timeout.as_secs().max(1);
    // The engine-wide ticker bumps the epoch every `EPOCH_TICK_MS`, so
    // setting `deadline_ticks = timeout_secs` gives second-granularity
    // enforcement on the configured `timeout`.
    store.set_epoch_deadline(timeout_secs);

    // Instantiation runs synchronously up to the first guest yield point and
    // could in principle spin; bound it with the same overall timeout.
    let service: Service =
        match tokio::time::timeout(timeout, service_pre.instantiate_async(&mut store)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                eprintln!("Instantiation failed: {e:?}");
                return Ok(error_response(500, format!("Instantiation failed:\n{e:?}")));
            }
            Err(_) => {
                eprintln!("Instantiation timed out after {timeout_secs}s");
                return Ok(error_response(
                    504,
                    format!("Instantiation timed out after {timeout_secs}s"),
                ));
            }
        };

    let (parts, body) = req.into_parts();
    let body = body.map_err(HttpErrorCode::from_hyper_request_error);
    let http_req = http::Request::from_parts(parts, body);
    let (wasi_req, io) = WasiRequest::from_http(http_req);

    let (resp_tx, resp_rx) = oneshot::channel::<HandlerOutcome>();
    let (frame_tx, frame_rx) = mpsc::channel::<Frame<Bytes>>(8);

    // The spawn task owns `store`, `service`, and `frame_tx`. Dropping
    // `frame_tx` (which happens automatically when the closure exits)
    // closes the body stream from hyper's point of view.
    tokio::spawn(async move {
        // Both `resp_tx` and `frame_rx` are taken at most once: when the
        // handler produces head frames it moves the receiver into the
        // outcome and releases the sender. After `run_concurrent` returns,
        // a still-`Some` `resp_tx` means we never produced a head and need
        // to surface the failure synthetically.
        let mut resp_tx = Some(resp_tx);
        let mut frame_rx_holder = Some(frame_rx);

        let drive_result: Result<anyhow::Result<()>, wasmtime::Error> = store
            .run_concurrent(async |store| -> anyhow::Result<()> {
                let handler = pin!(async {
                    let res = match service.handle(store, wasi_req).await? {
                        Ok(res) => res,
                        Err(err) => {
                            if let Some(tx) = resp_tx.take() {
                                let _ = tx.send(HandlerOutcome::GuestError(err));
                            }
                            return anyhow::Ok(());
                        }
                    };
                    let res = store.with(|store| res.into_http(store, async { Ok(()) }))?;
                    let (parts, body) = res.into_parts();

                    // Hand the response head plus the receiver to the outer
                    // task so hyper can start writing the response while we
                    // keep producing frames here.
                    if let Some(tx) = resp_tx.take() {
                        let rx = frame_rx_holder
                            .take()
                            .expect("frame_rx is taken at most once");
                        if tx.send(HandlerOutcome::Streaming(parts, rx)).is_err() {
                            // Outer side gave up before the head landed
                            // (e.g. first-byte timeout fired). Stop here;
                            // the body will be discarded.
                            return anyhow::Ok(());
                        }
                    }

                    // Pump frames until EOF, body error, or hyper drops the
                    // body. `frame_tx.send().await` blocks on a full channel,
                    // so a slow consumer back-pressures into the guest.
                    let mut body = pin!(body);
                    loop {
                        let frame = poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
                        match frame {
                            Some(Ok(frame)) => {
                                if frame_tx.send(frame).await.is_err() {
                                    // Hyper dropped the body; abandon the
                                    // rest of the stream.
                                    break;
                                }
                            }
                            // Body read error: close the stream by exiting
                            // the loop. `frame_tx` is dropped on return,
                            // which surfaces as EOF on the hyper side.
                            Some(Err(_)) => break,
                            None => break,
                        }
                    }
                    anyhow::Ok(())
                });
                let io_arm = pin!(async {
                    io.await
                        .map_err(|e| anyhow::anyhow!("request body I/O: {e}"))
                });
                match select(handler, io_arm).await {
                    Either::Left((res, _)) => res,
                    Either::Right((res, _)) => res,
                }
            })
            .await;

        // Surface failures that prevented the handler from producing a head.
        if let Some(tx) = resp_tx {
            let outcome = match drive_result {
                Ok(Ok(())) => HandlerOutcome::Trapped(
                    "Handler returned without producing a response".to_string(),
                ),
                Ok(Err(e)) => HandlerOutcome::Trapped(format!("Handler error: {e:?}")),
                Err(e) => {
                    if is_epoch_deadline_error(&e) {
                        HandlerOutcome::Timeout
                    } else {
                        HandlerOutcome::Trapped(format!("Handler trapped:\n{e:?}"))
                    }
                }
            };
            let _ = tx.send(outcome);
        }
    });

    // First-byte timeout. After the head arrives, the body stream is
    // bounded by the per-store epoch deadline rather than by this sleep.
    let timeout_fut = pin!(tokio::time::sleep(timeout));
    let outcome = match select(pin!(resp_rx), timeout_fut).await {
        Either::Left((Ok(outcome), _)) => outcome,
        Either::Left((Err(_recv), _)) => {
            HandlerOutcome::Trapped("Handler aborted without producing a response".to_string())
        }
        Either::Right((_, _resp_rx)) => HandlerOutcome::Timeout,
    };

    Ok(match outcome {
        HandlerOutcome::Streaming(parts, frame_rx) => {
            let body = ChannelBody { rx: frame_rx };
            HyperResponse::from_parts(parts, UnsyncBoxBody::new(body))
        }
        HandlerOutcome::GuestError(err) => error_response(500, format!("{err:?}")),
        HandlerOutcome::Trapped(msg) => {
            eprintln!("{msg}");
            error_response(500, msg)
        }
        HandlerOutcome::Timeout => {
            eprintln!("Handler timed out after {timeout_secs}s");
            error_response(504, format!("Handler timed out after {timeout_secs}s"))
        }
    })
}

fn is_epoch_deadline_error(err: &wasmtime::Error) -> bool {
    err.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::Interrupt)
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
