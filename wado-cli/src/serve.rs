use std::convert::Infallible;
use std::fmt::Write as _;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::{Either, poll_fn, select};
use futures::stream::FuturesUnordered;
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
use wasmtime::component::{Accessor, AccessorTask, Component};
use wasmtime::{Engine, Store};
use wasmtime_wasi_http::p3::Request as WasiRequest;
use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode as HttpErrorCode;
use wasmtime_wasi_http::p3::bindings::{Service, ServicePre};

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, OptLevel};
use crate::manifest;
use crate::runtime::{self, Preopens, WasiState};
use wado_compiler::LogLevel;

/// Default per-request timeout in seconds. A guest that fails to produce a
/// response head within this window is cut short by a first-byte timeout
/// (see `dispatch_request`).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Default number of requests a worker instance handles before it is
/// recycled (torn down and re-instantiated). Recycling resets state that
/// accumulates in a long-lived instance — chiefly the component resource
/// table (see issue #1133). Measured throughput is flat across recycle
/// thresholds from ~1k to ~100k requests, so this is sized for infrequent
/// recycling. `0` disables recycling.
const DEFAULT_RECYCLE_REQUESTS: u64 = 10000;

/// Default ceiling on concurrently in-flight requests. Each in-flight
/// request needs an async fiber stack from the pooling allocator, so this
/// also sizes the engine's stack pool.
const DEFAULT_MAX_CONCURRENCY: usize = 1024;

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
    /// Number of worker instances. `None` means "one per CPU".
    pub workers: Option<usize>,
    /// Recycle a worker after this many requests; `0` disables recycling.
    pub recycle_requests: u64,
    /// Ceiling on concurrently in-flight requests.
    pub max_concurrency: usize,
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
    Workers,
    RecycleRequests,
    MaxConcurrency,
    Help,
}

const TIMEOUT_SPEC: args::OptSpec = args::OptSpec {
    long: Some("timeout"),
    short: None,
    value: Some("<seconds>"),
    desc: "Per-request timeout in seconds (default: 30)",
};

const WORKERS_SPEC: args::OptSpec = args::OptSpec {
    long: Some("workers"),
    short: None,
    value: Some("<n>"),
    desc: "Number of worker instances (default: CPU count)",
};

const RECYCLE_REQUESTS_SPEC: args::OptSpec = args::OptSpec {
    long: Some("recycle-requests"),
    short: None,
    value: Some("<n>"),
    desc: "Recycle a worker after N requests; 0 disables (default: 10000)",
};

const MAX_CONCURRENCY_SPEC: args::OptSpec = args::OptSpec {
    long: Some("max-concurrency"),
    short: None,
    value: Some("<n>"),
    desc: "Max concurrently in-flight requests (default: 1024)",
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
        Self::Workers,
        Self::RecycleRequests,
        Self::MaxConcurrency,
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
            Self::Workers => WORKERS_SPEC,
            Self::RecycleRequests => RECYCLE_REQUESTS_SPEC,
            Self::MaxConcurrency => MAX_CONCURRENCY_SPEC,
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

/// Parse a non-negative integer option. When `allow_zero` is false, `0`
/// is rejected.
fn parse_count_arg(
    flag: &str,
    parser: &mut lexopt::Parser,
    allow_zero: bool,
) -> Result<u64, CliExit> {
    let s = args::require_string(parser)?;
    let n = s.parse::<u64>().map_err(|_| {
        CliExit::error(format!("{flag} requires a non-negative integer, got '{s}'"))
    })?;
    if n == 0 && !allow_zero {
        return Err(CliExit::error(format!("{flag} must be > 0")));
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
    let mut workers: Option<usize> = None;
    let mut recycle_requests: u64 = DEFAULT_RECYCLE_REQUESTS;
    let mut max_concurrency: usize = DEFAULT_MAX_CONCURRENCY;

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
                Opt::Workers => {
                    workers = Some(parse_count_arg("--workers", &mut parser, false)? as usize);
                }
                Opt::RecycleRequests => {
                    recycle_requests = parse_count_arg("--recycle-requests", &mut parser, true)?;
                }
                Opt::MaxConcurrency => {
                    max_concurrency =
                        parse_count_arg("--max-concurrency", &mut parser, false)? as usize;
                }
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
        workers,
        recycle_requests,
        max_concurrency,
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

/// A request handed to the engine task for processing on the shared
/// component instance.
struct RequestJob {
    wasi_req: WasiRequest,
    io: Pin<Box<dyn Future<Output = Result<(), HttpErrorCode>> + Send>>,
    resp_tx: oneshot::Sender<HandlerOutcome>,
}

/// Processes one HTTP request on the long-lived component instance.
///
/// The component is instantiated once at server startup; every request
/// runs as a task spawned (via `Accessor::spawn`) onto that single shared
/// instance. Guest state therefore persists across requests — managing it
/// is the Wado program's responsibility, exactly as for a conventional
/// HTTP server daemon.
///
/// As soon as the response head is available the task hands it — plus a
/// body-frame channel — back to `dispatch_request` over a oneshot; from
/// then on hyper drains body frames directly from that channel while the
/// task keeps producing them. The channel is bounded (capacity 8), so a
/// slow consumer back-pressures the guest.
struct HandlerTask {
    service: Arc<Service>,
    job: RequestJob,
}

impl AccessorTask<WasiState> for HandlerTask {
    async fn run(self, accessor: &Accessor<WasiState>) -> wasmtime::Result<()> {
        let HandlerTask { service, job } = self;
        let RequestJob {
            wasi_req,
            io,
            resp_tx,
        } = job;

        let mut resp_tx = Some(resp_tx);
        let (frame_tx, frame_rx) = mpsc::channel::<Frame<Bytes>>(8);
        let mut frame_rx_holder = Some(frame_rx);

        // Scoped so the pinned `handler`/`io_arm` coroutines — which borrow
        // `resp_tx` — are dropped before `resp_tx` is inspected below.
        let drive_result = {
            let handler = pin!(async {
                let res = match service.handle(accessor, wasi_req).await? {
                    Ok(res) => res,
                    Err(err) => {
                        if let Some(tx) = resp_tx.take() {
                            let _ = tx.send(HandlerOutcome::GuestError(err));
                        }
                        return anyhow::Ok(());
                    }
                };
                let res = accessor.with(|store| res.into_http(store, async { Ok(()) }))?;
                let (parts, body) = res.into_parts();

                // Hand the response head plus the receiver to `dispatch_request`
                // so hyper can start writing the response while we keep
                // producing body frames here.
                if let Some(tx) = resp_tx.take() {
                    let rx = frame_rx_holder
                        .take()
                        .expect("frame_rx is taken at most once");
                    if tx.send(HandlerOutcome::Streaming(parts, rx)).is_err() {
                        // Caller gave up before the head landed (e.g. first-byte
                        // timeout fired). Stop here; the body is discarded.
                        return anyhow::Ok(());
                    }
                }

                // Pump frames until EOF, body error, or hyper drops the body.
                // `frame_tx.send().await` blocks on a full channel, so a slow
                // consumer back-pressures into the guest.
                let mut body = pin!(body);
                loop {
                    match poll_fn(|cx| body.as_mut().poll_frame(cx)).await {
                        Some(Ok(frame)) => {
                            if frame_tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        Some(Err(_)) | None => break,
                    }
                }
                anyhow::Ok(())
            });
            let io_arm = pin!(async {
                io.await
                    .map_err(|e| anyhow::anyhow!("request body I/O: {e:?}"))
            });
            // The handler is the source of truth for response progress; the io
            // future only drives request-body delivery and typically resolves
            // later. Poll io concurrently, but never wait for it alone.
            match select(handler, io_arm).await {
                Either::Left((res, _io)) => res,
                Either::Right((_io_res, handler)) => handler.await,
            }
        };

        // Surface failures that prevented a response head. After the head
        // has been sent, `resp_tx` is `None` and we just exit.
        if let Some(tx) = resp_tx {
            let outcome = match drive_result {
                Ok(()) => HandlerOutcome::Trapped(
                    "Handler returned without producing a response".to_string(),
                ),
                Err(e) => HandlerOutcome::Trapped(format!("Handler error: {e:?}")),
            };
            let _ = tx.send(outcome);
        }
        Ok(())
    }
}

/// Round-robin dispatcher over a fixed pool of worker instances. Each
/// worker is one long-lived component instance bound to its own store and
/// engine task; requests are striped across them so guest execution fans
/// out over multiple cores instead of serialising on one.
struct Dispatch {
    txs: Vec<mpsc::Sender<RequestJob>>,
    next: AtomicUsize,
}

impl Dispatch {
    /// Hand `job` to the next worker in rotation. Errors only once that
    /// worker's engine task has stopped (i.e. during shutdown).
    async fn submit(&self, job: RequestJob) -> Result<(), mpsc::error::SendError<RequestJob>> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.txs.len();
        self.txs[idx].send(job).await
    }
}

/// Convert a hyper request into a job, submit it to a worker instance, and
/// render the outcome as a hyper response.
///
/// A first-byte `tokio::time::sleep(timeout)` is raced against the head
/// oneshot, so a guest that never produces a response head yields a 504.
async fn dispatch_request(
    dispatch: &Dispatch,
    timeout: Duration,
    req: HyperRequest<hyper::body::Incoming>,
) -> Result<HyperResponse<StreamingBody>> {
    let (parts, body) = req.into_parts();
    let body = body.map_err(HttpErrorCode::from_hyper_request_error);
    let http_req = http::Request::from_parts(parts, body);
    let (wasi_req, io) = WasiRequest::from_http(http_req);

    let (resp_tx, resp_rx) = oneshot::channel::<HandlerOutcome>();
    let job = RequestJob {
        wasi_req,
        io: Box::pin(io),
        resp_tx,
    };

    if dispatch.submit(job).await.is_err() {
        return Ok(error_response(503, "Server is shutting down".to_string()));
    }

    // First-byte timeout. After the head arrives, the body stream is bounded
    // by the bounded frame channel rather than by this sleep.
    let timeout_fut = pin!(tokio::time::sleep(timeout));
    let outcome = match select(pin!(resp_rx), timeout_fut).await {
        Either::Left((Ok(outcome), _)) => outcome,
        Either::Left((Err(_recv), _)) => {
            HandlerOutcome::Trapped("Handler aborted without producing a response".to_string())
        }
        Either::Right(((), _resp_rx)) => HandlerOutcome::Timeout,
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
            let secs = timeout.as_secs();
            eprintln!("Handler timed out after {secs}s");
            error_response(504, format!("Handler timed out after {secs}s"))
        }
    })
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

/// Why a worker's dispatch loop stopped.
#[derive(Clone, Copy)]
enum WorkerStop {
    /// Recycle threshold reached: tear the instance down and rebuild.
    Recycle,
    /// The request channel closed: the server is shutting down.
    Shutdown,
}

/// One step of a worker's dispatch loop.
enum Step {
    /// A job arrived (`Some`), or the request channel closed (`None`).
    Job(Option<RequestJob>),
    /// An in-flight request finished.
    Drained,
}

/// Drives one worker: a long-lived component instance that handles
/// requests off `job_rx`.
///
/// After `recycle_requests` requests the instance is torn down and
/// replaced with a fresh one. Recycling bounds the per-request garbage
/// that piles up in a worker's long-lived Wasm GC heap; the pooling
/// allocator makes the rebuild cheap by reusing the OS memory slots.
/// `recycle_requests == 0` disables recycling (reuse forever).
///
/// On recycle or shutdown the loop stops accepting new requests but keeps
/// draining the in-flight ones, so a recycle never drops a live request.
async fn worker_loop(
    engine: Engine,
    service_pre: Arc<ServicePre<WasiState>>,
    preopens: Arc<Preopens>,
    mut job_rx: mpsc::Receiver<RequestJob>,
    recycle_requests: u64,
) {
    loop {
        // Fresh instance for this generation. Guest module-init (Wado
        // `global` initializers, e.g. a router built at startup) runs
        // here — once per generation, not once per request.
        let state = WasiState::new_no_inherit_env_with_preopens(&preopens, &[]);
        let mut store = Store::new(&engine, state);
        // The serve engine enables epoch interruption; a long-lived store
        // has no per-request deadline to enforce, so push it out of reach.
        store.set_epoch_deadline(u64::MAX);
        let service = match service_pre.instantiate_async(&mut store).await {
            Ok(service) => Arc::new(service),
            Err(e) => {
                eprintln!("Worker instantiation failed: {e:?}");
                return;
            }
        };

        let stop = store
            .run_concurrent(async |accessor| {
                let mut inflight = FuturesUnordered::new();
                let mut handled: u64 = 0;
                let mut stopping: Option<WorkerStop> = None;
                loop {
                    if let Some(stop) = stopping
                        && inflight.is_empty()
                    {
                        return stop;
                    }

                    let step = if stopping.is_some() {
                        // Draining only — `inflight` is non-empty here.
                        let _ = inflight.next().await;
                        Step::Drained
                    } else if inflight.is_empty() {
                        // Idle — only a new job can wake us.
                        Step::Job(job_rx.recv().await)
                    } else {
                        // Accept new jobs and drain in-flight concurrently.
                        let recv = pin!(job_rx.recv());
                        let drain = pin!(inflight.next());
                        match select(recv, drain).await {
                            Either::Left((job, _drain)) => Step::Job(job),
                            Either::Right((_done, _recv)) => Step::Drained,
                        }
                    };

                    match step {
                        Step::Drained => {}
                        Step::Job(Some(job)) => {
                            // The `JoinHandle` is kept in `inflight` so a
                            // recycle can wait for the request to finish;
                            // the task itself is driven by `run_concurrent`.
                            inflight.push(accessor.spawn(HandlerTask {
                                service: Arc::clone(&service),
                                job,
                            }));
                            handled += 1;
                            if recycle_requests != 0 && handled >= recycle_requests {
                                stopping = Some(WorkerStop::Recycle);
                            }
                        }
                        Step::Job(None) => stopping = Some(WorkerStop::Shutdown),
                    }
                }
            })
            .await;

        match stop {
            // Drop the old store (returns its pooling slots) and loop to
            // build a fresh instance.
            Ok(WorkerStop::Recycle) => {}
            Ok(WorkerStop::Shutdown) => return,
            Err(e) => {
                eprintln!("Worker engine error: {e:?}");
                return;
            }
        }
    }
}

async fn run_http_server(
    wasm: Vec<u8>,
    addr: &str,
    cranelift_opt: wasmtime::OptLevel,
    preopened_dirs: Vec<(String, String)>,
    timeout: Duration,
    workers: usize,
    recycle_requests: u64,
    max_concurrency: usize,
) -> Result<()> {
    // Pool head-room: at most `workers` instances are live at once (a
    // recycle drops the old instance before building the new), plus slack.
    let max_instances = u32::try_from(workers).unwrap_or(u32::MAX).saturating_add(8);
    let engine = runtime::create_serve_engine(
        cranelift_opt,
        max_instances,
        u32::try_from(max_concurrency).unwrap_or(u32::MAX),
    )?;
    let component = Component::new(&engine, &wasm)?;
    let linker = runtime::create_linker(&engine)?;
    // Open preopens once at startup; they are attached to every worker
    // generation's `WasiState`.
    let preopens = Arc::new(Preopens::open(&preopened_dirs)?);
    let instance_pre = linker.instantiate_pre(&component)?;
    let service_pre = Arc::new(ServicePre::<WasiState>::new(instance_pre)?);
    drop(linker);
    drop(component);

    // One worker = one long-lived component instance on its own store and
    // engine task. Workers run guest code on independent stores, so
    // request handling fans out across cores; each worker recycles its
    // instance every `recycle_requests` requests.
    let chan_cap = (max_concurrency / workers).max(16);
    let mut txs = Vec::with_capacity(workers);
    let mut engine_tasks = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (tx, rx) = mpsc::channel::<RequestJob>(chan_cap);
        txs.push(tx);
        engine_tasks.push(tokio::spawn(worker_loop(
            engine.clone(),
            Arc::clone(&service_pre),
            Arc::clone(&preopens),
            rx,
            recycle_requests,
        )));
    }
    let dispatch = Arc::new(Dispatch {
        txs,
        next: AtomicUsize::new(0),
    });

    let addr: SocketAddr = addr.parse()?;
    let listener = TcpListener::bind(addr).await?;

    let recycle_desc = if recycle_requests == 0 {
        "off".to_string()
    } else {
        format!("every {recycle_requests} req")
    };
    eprintln!("HTTP server listening on http://{addr}/");
    eprintln!("Instance reuse: ON — {workers} worker(s), recycle {recycle_desc}");
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
                // A persistent failure (fd exhaustion, listener gone) would
                // otherwise turn the loop into a hot CPU/log spammer. A short
                // sleep is a cheap circuit-breaker.
                eprintln!("accept error: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Either::Left((Ok((stream, remote_addr)), _shutdown)) => {
                let io = TokioIo::new(stream);
                let dispatch = Arc::clone(&dispatch);

                connections.spawn(async move {
                    let svc = service_fn(move |req| {
                        let dispatch = Arc::clone(&dispatch);
                        async move { dispatch_request(&dispatch, timeout, req).await }
                    });

                    // Auto-detect HTTP/1.1 vs h2c by sniffing the connection
                    // preface, so h2c clients get trailers without the
                    // `TE: trailers` handshake hyper requires on HTTP/1.
                    let builder = auto::Builder::new(TokioExecutor::new());
                    if let Err(e) = builder.serve_connection(io, svc).await {
                        eprintln!("Error serving {remote_addr}: {e}");
                    }
                });

                while let Some(res) = connections.try_join_next() {
                    log_connection_join_error(res);
                }
            }
        }
    }

    // Drain in-flight connections first — they still need the worker engine
    // tasks alive to receive their responses.
    let drain_deadline = timeout + Duration::from_secs(5);
    let drain = async {
        while let Some(res) = connections.join_next().await {
            log_connection_join_error(res);
        }
    };
    if tokio::time::timeout(drain_deadline, drain).await.is_err() {
        eprintln!(
            "Drain timeout after {}s; aborting remaining connections",
            drain_deadline.as_secs()
        );
        connections.shutdown().await;
    }

    // No connection can submit more work now: dropping the dispatcher
    // closes every worker channel, ending each engine task's dispatch
    // loop. Wait for them to wind down.
    drop(dispatch);
    for engine_task in engine_tasks {
        let _ = tokio::time::timeout(drain_deadline, engine_task).await;
    }

    Ok(())
}

/// Log connection task panics; ignore cancellation (which is the normal
/// outcome when shutdown aborts in-flight tasks at the drain deadline).
fn log_connection_join_error(res: Result<(), tokio::task::JoinError>) {
    if let Err(e) = res
        && !e.is_cancelled()
    {
        eprintln!("connection task panicked: {e}");
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

    let timeout = Duration::from_secs(opts.timeout_secs);
    let workers = opts.workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    if let Err(e) = run_http_server(
        wasm,
        &opts.addr,
        cranelift_opt,
        opts.preopened_dirs,
        timeout,
        workers,
        opts.recycle_requests,
        opts.max_concurrency,
    )
    .await
    {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
