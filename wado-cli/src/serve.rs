use std::convert::Infallible;
use std::fmt::Write as _;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use futures::Stream;
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
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use wasmtime::component::{Accessor, AccessorTask, Component};
use wasmtime::{AsContextMut, Engine, GuestProfiler, Store, UpdateDeadline};
use wasmtime_wasi_http::p3::Request as WasiRequest;
use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode as HttpErrorCode;
use wasmtime_wasi_http::p3::bindings::{Service, ServicePre};

use crate::args::{self, CliExit};
use crate::build::build_for_driver;
use crate::compile::CompileFlags;
use crate::knobs::{CompileKnobs, KnobOpt};
use crate::manifest;
use crate::runtime::{self, Preopens, ProfileMode, WasiState};
use crate::sync::lock;

/// First-byte timeout cuts off guests stuck in the host; the epoch
/// deadline (see `worker_loop`) catches runaway pure-wasm loops.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Engine epoch is bumped every tick; deadlines count ticks, so this
/// is the granularity of runaway-guest detection.
const EPOCH_TICK_MS: u64 = 1000;

/// Recycling resets state that accumulates in a long-lived instance, notably
/// the component resource table (issue #1133). Throughput peaks over a broad
/// 25–200 plateau, tuned on a service whose `global` initializers are cheap;
/// raise it for one that pays to build them. `0` disables.
const DEFAULT_RECYCLE_REQUESTS: u64 = 200;

/// In-flight requests one worker runs at once. Each pins a fiber stack the
/// GC walks on every collection, so throughput peaks over a 16–32 plateau
/// and a worker left to run the whole backlog loses a third of it by 200.
const DEFAULT_MAX_CONCURRENCY_PER_WORKER: usize = 32;

pub struct ServeOptions {
    pub input: String,
    pub knobs: CompileKnobs,
    pub addr: String,
    pub collector: wasmtime::Collector,
    /// Empty by default: unlike `wado run`, services don't preopen cwd
    /// automatically — the user must pass `--dir`.
    pub preopened_dirs: Vec<(String, String)>,
    pub timeout_secs: u64,
    /// `None` ⇒ one worker per CPU.
    pub workers: Option<usize>,
    /// Recycle a worker after this many requests; `0` disables.
    pub recycle_requests: u64,
    /// Server-wide in-flight bound; `None` ⇒
    /// `DEFAULT_MAX_CONCURRENCY_PER_WORKER` per worker.
    pub max_concurrency: Option<usize>,
    pub profile: ProfileMode,
}

#[derive(Clone, Copy)]
enum Opt {
    Addr,
    Dir,
    Collector,
    Timeout,
    Workers,
    RecycleRequests,
    MaxConcurrency,
    Profile,
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
    desc: "Recycle a worker after N requests; raise it when the service's\ninitializers are costly, 0 disables (default: 200)",
};

const MAX_CONCURRENCY_SPEC: args::OptSpec = args::OptSpec {
    long: Some("max-concurrency"),
    short: None,
    value: Some("<n>"),
    desc: "Max concurrently in-flight requests server-wide; the surplus waits\nin the worker queues (default: 32 per worker)",
};

const PROFILE_SPEC: args::OptSpec = args::OptSpec {
    long: Some("profile"),
    short: None,
    value: Some("<mode>"),
    desc: "Enable guest profiling (forces --workers 1):\n  guest[,path[,interval_ms]]  Cross-platform guest profiling\n                               (default: profile.json, 10ms)\nThe profile is written on shutdown (Ctrl-C).",
};

impl Opt {
    const ALL: &[Self] = &[
        Self::Addr,
        Self::Dir,
        Self::Collector,
        Self::Timeout,
        Self::Workers,
        Self::RecycleRequests,
        Self::MaxConcurrency,
        Self::Profile,
        Self::Help,
    ];

    const KNOBS: &[KnobOpt] = &[
        KnobOpt::OptLevel,
        KnobOpt::InlineThreshold,
        KnobOpt::InlineGrowth,
        KnobOpt::OptIterations,
        KnobOpt::LogLevel,
        KnobOpt::Allocator,
        KnobOpt::NoCache,
        KnobOpt::Feature,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Addr => args::OptSpec {
                long: Some("addr"),
                short: None,
                value: Some("<addr>"),
                desc: "Address to listen on (default: 0.0.0.0:8080)",
            },
            // No `--no-dir`, unlike `run`/`test`: a service preopens nothing
            // by default, so it would have nothing to disable.
            Self::Dir => args::DIR_SPEC,
            Self::Collector => args::COLLECTOR_SPEC,
            Self::Timeout => TIMEOUT_SPEC,
            Self::Workers => WORKERS_SPEC,
            Self::RecycleRequests => RECYCLE_REQUESTS_SPEC,
            Self::MaxConcurrency => MAX_CONCURRENCY_SPEC,
            Self::Profile => PROFILE_SPEC,
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
    writeln!(
        buf,
        "The listener serves HTTP/1.1 and cleartext HTTP/2 (h2c) on the same port,\n\
         choosing per connection from the client's opening bytes — there is no flag\n\
         and no upgrade handshake. An h2c client always receives response trailers;\n\
         an HTTP/1.1 client receives only the fields the response names in its\n\
         `Trailer` header, and only when the request sent `TE: trailers`. TLS is\n\
         not terminated here; put a reverse proxy in front for HTTPS."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(
        buf,
        "{}",
        args::OptsHelp::default()
            .add(Opt::ALL, |o| o.spec())
            .add(Opt::KNOBS, |o| o.spec())
            .add(args::ParamOpt::ALL, |o| o.spec())
            .render()
    )
    .unwrap();
    buf
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

/// Parse a count that also has to fit the pooling allocator's `u32` instance /
/// stack counts.
fn parse_u32_count_arg(flag: &str, parser: &mut lexopt::Parser) -> Result<usize, CliExit> {
    let n = parse_count_arg(flag, parser, false)?;
    let n = u32::try_from(n).map_err(|_| CliExit::error(format!("{flag} value is too large")))?;
    Ok(n as usize)
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<ServeOptions, CliExit> {
    let usage = format_usage();
    let mut input: Option<String> = None;
    let mut addr = "0.0.0.0:8080".to_string();
    let mut collector = runtime::DEFAULT_COLLECTOR;
    let mut preopened_dirs: Vec<(String, String)> = Vec::new();
    let mut timeout_secs: u64 = DEFAULT_TIMEOUT_SECS;
    let mut workers: Option<usize> = None;
    let mut recycle_requests: u64 = DEFAULT_RECYCLE_REQUESTS;
    let mut max_concurrency: Option<usize> = None;
    let mut profile = ProfileMode::None;
    let mut knobs = CompileKnobs::default();

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(p) = args::match_opt(&arg, args::ParamOpt::ALL, |p| p.spec()) {
            knobs.params.apply(p, &mut parser)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Addr => addr = args::require_string(&mut parser)?,
                Opt::Dir => preopened_dirs.push(args::parse_dir_arg(&mut parser)?),
                Opt::Collector => {
                    let spec = args::require_string(&mut parser)?;
                    collector = runtime::parse_collector(&spec).map_err(CliExit::error)?;
                }
                Opt::Timeout => timeout_secs = parse_count_arg("--timeout", &mut parser, false)?,
                Opt::Workers => {
                    workers = Some(parse_u32_count_arg("--workers", &mut parser)?);
                }
                Opt::RecycleRequests => {
                    recycle_requests = parse_count_arg("--recycle-requests", &mut parser, true)?;
                }
                Opt::MaxConcurrency => {
                    max_concurrency = Some(parse_u32_count_arg("--max-concurrency", &mut parser)?);
                }
                Opt::Profile => {
                    let spec = args::require_string(&mut parser)?;
                    profile = runtime::parse_profile(&spec)?;
                    if !matches!(profile, ProfileMode::Guest { .. }) {
                        return Err(CliExit::error(
                            "wado serve supports only --profile guest".to_string(),
                        ));
                    }
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

    // More workers than the bound would leave one without a slot. Only both
    // explicit can collide: a derived worker count is clamped in `run`, and a
    // derived bound scales with the workers.
    if let Some(w) = workers
        && let Some(max_concurrency) = max_concurrency
        && w > max_concurrency
    {
        return Err(CliExit::error(format!(
            "--workers ({w}) must not exceed --max-concurrency ({max_concurrency})"
        )));
    }

    Ok(ServeOptions {
        input: manifest::resolve_input(input, manifest::EntryPointKind::Service, &usage)?,
        knobs,
        addr,
        collector,
        preopened_dirs,
        timeout_secs,
        workers,
        recycle_requests,
        max_concurrency,
        profile,
    })
}

/// `Streaming` is the success path — head + body receiver. The other
/// variants are pre-head failures that must be rendered as a synthetic 5xx.
enum HandlerOutcome {
    Streaming(http::response::Parts, mpsc::Receiver<Frame<Bytes>>),
    GuestError(wasmtime_wasi_http::p3::bindings::http::types::ErrorCode),
    Trapped(String),
    Timeout,
}

/// `http_body::Body` over a tokio `mpsc::Receiver` — frames flow one at a
/// time, so the full body never sits in memory. Trailers piggyback on the
/// same channel as `Frame::trailers(...)`.
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

struct RequestJob {
    wasi_req: WasiRequest,
    io: Pin<Box<dyn Future<Output = Result<(), HttpErrorCode>> + Send>>,
    resp_tx: oneshot::Sender<HandlerOutcome>,
}

/// One HTTP request, spawned onto the long-lived component instance, so
/// guest state persists across requests like a conventional server daemon.
struct HandlerTask {
    service: Arc<Service>,
    job: RequestJob,
    /// Per-frame idle timeout on the body pump. A non-draining client
    /// (issue #1138) cannot pin this task's fiber stack indefinitely;
    /// a slow-but-progressing client is unaffected.
    idle_timeout: Duration,
}

impl AccessorTask<WasiState> for HandlerTask {
    async fn run(self, accessor: &Accessor<WasiState>) -> wasmtime::Result<()> {
        let HandlerTask {
            service,
            job,
            idle_timeout,
        } = self;
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
                // The head's consumer bounds the handler. Past a 504 or a
                // disconnect nobody will read the response, and the epoch
                // deadline cannot reclaim a guest parked in a host call,
                // because it runs no wasm.
                let called = {
                    let abandoned = pin!(async {
                        resp_tx
                            .as_mut()
                            .expect("resp_tx is taken only after the head is sent")
                            .closed()
                            .await;
                    });
                    match select(pin!(service.handle(accessor, wasi_req)), abandoned).await {
                        Either::Left((called, _abandoned)) => called?,
                        Either::Right(((), _call)) => return anyhow::Ok(()),
                    }
                };
                let res = match called {
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
                let mut body = pin!(body);

                // Hold the head for one turn of the store's event loop, so a
                // body the guest has already finished rides out with it in one
                // `writev`. One turn is the whole wait.
                let mut first_frame = None;
                let mut body_done = false;
                for turn in 0..2 {
                    if turn > 0 {
                        tokio::task::yield_now().await;
                    }
                    match poll_fn(|cx| Poll::Ready(body.as_mut().poll_frame(cx))).await {
                        Poll::Ready(Some(Ok(frame))) => {
                            first_frame = Some(frame);
                            break;
                        }
                        Poll::Ready(Some(Err(_)) | None) => {
                            body_done = true;
                            break;
                        }
                        Poll::Pending => {}
                    }
                }
                if let Some(frame) = first_frame {
                    // Queued before the head, which is what hands hyper the
                    // receiver: after it, hyper races us to the first poll.
                    assert!(
                        frame_tx.try_send(frame).is_ok(),
                        "the frame channel is empty and its receiver held until the head is sent"
                    );
                }

                // Hand the head over; hyper writes it while we pump the body.
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

                // Pump frames until EOF, a body error, or hyper dropping the
                // body. Room in the channel means `try_send` completes without
                // arming a timer; a full one back-pressures into the guest.
                while !body_done
                    && let Some(Ok(frame)) = poll_fn(|cx| body.as_mut().poll_frame(cx)).await
                {
                    match frame_tx.try_send(frame) {
                        Ok(()) => {}
                        // hyper dropped the body — client disconnected.
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                        // Channel full: back-pressure, bounded by the idle
                        // timeout so a stalled consumer cannot pin the fiber.
                        Err(mpsc::error::TrySendError::Full(frame)) => {
                            match tokio::time::timeout(idle_timeout, frame_tx.send(frame)).await {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => break,
                                Err(_) => {
                                    eprintln!(
                                        "Request body pump aborted: response consumer \
                                         stalled for more than {idle_timeout:?}"
                                    );
                                    break;
                                }
                            }
                        }
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
    /// Coarse server-wide clock for the first-byte timeout. Each in-flight
    /// request clones this and checks its own deadline on every tick (see
    /// `before_deadline`), so no request arms its own timer.
    tick_rx: watch::Receiver<()>,
}

/// Why a job never reached a worker. Carries nothing: `SendError` would hand
/// back the whole `RequestJob`.
enum SubmitError {
    /// The chosen worker's engine task has stopped, i.e. the server is
    /// shutting down.
    WorkerGone,
    /// The worker's queue stayed full past the request's deadline.
    Timeout,
}

impl Dispatch {
    /// Hand `job` to the next worker in rotation, giving up at `deadline`.
    /// Waiting for a full queue is part of the client's time to first byte,
    /// so it answers to the same deadline as the handler.
    async fn submit(&self, job: RequestJob, deadline: Instant) -> Result<(), SubmitError> {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.txs.len();
        let mut tick_rx = self.tick_rx.clone();
        match before_deadline(pin!(self.txs[idx].send(job)), deadline, &mut tick_rx).await {
            Some(sent) => sent.map_err(|_| SubmitError::WorkerGone),
            None => Err(SubmitError::Timeout),
        }
    }
}

/// Drive `fut` to completion, giving up once `deadline` has passed. Waiters
/// share the server tick rather than arming a timer each, so the answer lands
/// within a tick after the deadline, never before it.
async fn before_deadline<F: Future>(
    mut fut: Pin<&mut F>,
    deadline: Instant,
    tick_rx: &mut watch::Receiver<()>,
) -> Option<F::Output> {
    loop {
        match select(fut.as_mut(), pin!(tick_rx.changed())).await {
            Either::Left((out, _tick)) => return (Instant::now() < deadline).then_some(out),
            Either::Right((Ok(()), _fut)) => {
                if Instant::now() >= deadline {
                    return None;
                }
            }
            // Ticker gone: the server is shutting down, and shutdown drains
            // what is in flight, so stop checking and let `fut` finish.
            Either::Right((Err(_closed), _fut)) => break,
        }
    }
    Some(fut.await)
}

/// Convert a hyper request into a job, run it on a worker, and render the
/// outcome. One deadline covers the whole path to the response head, and
/// leaving here drops `resp_rx`, which ends a handler still producing it.
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

    let deadline = Instant::now() + timeout;
    let timed_out = || {
        let secs = timeout.as_secs();
        eprintln!("Handler timed out after {secs}s");
        error_response(504, format!("Handler timed out after {secs}s"))
    };
    match dispatch.submit(job, deadline).await {
        Ok(()) => {}
        Err(SubmitError::WorkerGone) => {
            return Ok(error_response(503, "Server is shutting down".to_string()));
        }
        Err(SubmitError::Timeout) => return Ok(timed_out()),
    }

    // First-byte timeout. After the head arrives, the body stream is bounded
    // by the bounded frame channel rather than by this deadline.
    let mut tick_rx = dispatch.tick_rx.clone();
    let outcome = match before_deadline(pin!(resp_rx), deadline, &mut tick_rx).await {
        Some(Ok(outcome)) => outcome,
        Some(Err(_recv)) => {
            HandlerOutcome::Trapped("Handler aborted without producing a response".to_string())
        }
        None => HandlerOutcome::Timeout,
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
        HandlerOutcome::Timeout => timed_out(),
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

/// Shared guest-profiler handle for `--profile guest`. Component-scoped, so
/// it outlives recycling and each new store re-registers against it.
#[derive(Clone)]
struct GuestProfilerHandle {
    profiler: Arc<Mutex<Option<GuestProfiler>>>,
    interval: Duration,
}

/// One step of a worker's dispatch loop.
enum Step {
    /// A job slot was produced into the loop's `job` local: `Some` is a
    /// request to dispatch, `None` means the request channel closed.
    Job,
    /// Nothing to dispatch: an in-flight request finished, or the wait hit
    /// the server tick. Either way the loop turned.
    Turn,
}

/// Wait for an in-flight request to finish or for the next server tick,
/// whichever comes first. Every wait a busy worker makes goes through here,
/// so its loop keeps turning even when nothing finishes.
async fn drain_or_tick<S: Stream + Unpin>(inflight: &mut S, tick_rx: &mut watch::Receiver<()>) {
    // Once the ticker is gone (shutdown) only the drain arm can resolve.
    let tick = pin!(async {
        if tick_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    });
    let _ = select(pin!(inflight.next()), tick).await;
}

/// Drives one worker: a long-lived component instance serving `job_rx`,
/// rebuilt every `recycle_requests` requests and after a trap. It runs
/// `max_inflight` requests at once and leaves the rest queued.
async fn worker_loop(
    engine: Engine,
    service_pre: Arc<ServicePre<WasiState>>,
    preopens: Arc<Preopens>,
    mut job_rx: mpsc::Receiver<RequestJob>,
    max_inflight: usize,
    mut tick_rx: watch::Receiver<()>,
    recycle_requests: u64,
    timeout_secs: u64,
    fatal: Arc<Notify>,
    profiler: Option<GuestProfilerHandle>,
) {
    // Grace over the client-facing timeout and the 1s tick granularity, so
    // the first-byte 504 precedes the epoch trap. Under `--profile guest`
    // every tick must reach the sampling callback, which turns the trap off
    // for the session.
    let epoch_ticks = if profiler.is_some() {
        1
    } else {
        timeout_secs.saturating_add(5)
    };
    // Idle timeout handed to every `HandlerTask` for its body pump.
    let request_timeout = Duration::from_secs(timeout_secs);
    loop {
        // Fresh instance for this generation. Guest module-init (Wado
        // `global` initializers, e.g. a router built at startup) runs
        // here — once per generation, not once per request.
        let state = WasiState::new_no_inherit_env_with_preopens(&preopens, &[]);
        let mut store = Store::new(&engine, state);
        // Arm the epoch deadline. The dispatch loop refreshes it every
        // turn (see below), so a healthy worker never trips it; a guest
        // that runs away in pure wasm does.
        store.set_epoch_deadline(epoch_ticks);
        // Under `--profile guest`, sample the profiler on every epoch
        // tick. The callback re-arms the deadline at 1 so it fires again
        // next tick.
        if let Some(ref handle) = profiler {
            let profiler = Arc::clone(&handle.profiler);
            let interval = handle.interval;
            store.epoch_deadline_callback(move |store_ctx| {
                if let Some(ref mut p) = *lock(&profiler) {
                    p.sample(&store_ctx, interval);
                }
                Ok(UpdateDeadline::Continue(1))
            });
        }
        let service = match service_pre.instantiate_async(&mut store).await {
            Ok(service) => Arc::new(service),
            Err(e) => {
                // `Component::new` and `instantiate_pre` already succeeded at
                // startup, so this fault would hit every worker identically:
                // the server cannot serve. Shut down rather than leave a dead
                // worker whose channel still accepts jobs.
                eprintln!("Worker instantiation failed: {e:?}");
                fatal.notify_one();
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
                        return Ok(stop);
                    }

                    let mut job: Option<RequestJob> = None;
                    let step = if inflight.len() >= max_inflight || stopping.is_some() {
                        // At the bound, or draining towards a stop: take
                        // nothing new, so the connections holding the queued
                        // requests back-pressure.
                        assert!(!inflight.is_empty());
                        drain_or_tick(&mut inflight, &mut tick_rx).await;
                        Step::Turn
                    } else if inflight.is_empty() {
                        // Idle — only a new job can wake us. No guest code is
                        // running, so the epoch deadline cannot trip.
                        job = job_rx.recv().await;
                        Step::Job
                    } else {
                        // Accept new jobs and drain in-flight concurrently.
                        let recv = pin!(job_rx.recv());
                        let drain = pin!(drain_or_tick(&mut inflight, &mut tick_rx));
                        match select(recv, drain).await {
                            Either::Left((j, _drain)) => {
                                job = j;
                                Step::Job
                            }
                            Either::Right(((), _recv)) => Step::Turn,
                        }
                    };

                    // The loop turned, so the worker is not starved by a
                    // runaway guest: push the epoch deadline back out.
                    accessor.with(|mut access| {
                        access.as_context_mut().set_epoch_deadline(epoch_ticks);
                    });

                    match step {
                        Step::Turn => {}
                        Step::Job => match job {
                            Some(job) => {
                                // The `JoinHandle` is kept in `inflight` so a
                                // recycle can wait for the request to finish;
                                // the task itself is driven by `run_concurrent`.
                                inflight.push(accessor.spawn(HandlerTask {
                                    service: Arc::clone(&service),
                                    job,
                                    idle_timeout: request_timeout,
                                })?);
                                handled += 1;
                                if recycle_requests != 0 && handled >= recycle_requests {
                                    stopping = Some(WorkerStop::Recycle);
                                }
                            }
                            None => stopping = Some(WorkerStop::Shutdown),
                        },
                    }
                }
            })
            .await
            .and_then(|inner| inner);

        match stop {
            // Drop the old store (returns its pooling slots) and loop to
            // build a fresh instance.
            Ok(WorkerStop::Recycle) => {}
            Ok(WorkerStop::Shutdown) => return,
            Err(e) => {
                // Rebuild so the worker self-heals. Requests in flight on the
                // trapped store are lost; queued ones survive. The sleep
                // bounds a guest that traps on every request.
                eprintln!("Worker instance trapped; rebuilding: {e:?}");
                tokio::time::sleep(Duration::from_millis(100)).await;
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
    profile: ProfileMode,
    collector: wasmtime::Collector,
) -> Result<()> {
    // `workers` is bounded to `u32` range in `parse_args` and
    // `max_concurrency` in `run`, so these conversions never fail.
    let workers_u32 = u32::try_from(workers).expect("workers bounded to u32 in parse_args");
    let max_concurrency_u32 =
        u32::try_from(max_concurrency).expect("max_concurrency bounded to u32 in run");
    // Pool head-room: at most `workers` instances are live at once (a
    // recycle drops the old instance before building the new), plus slack.
    let max_instances = workers_u32.saturating_add(8);
    // Two stacks per in-flight request: a cancelled handler returns its slot
    // while the thread it dropped is still unwinding, and a peer closing many
    // connections at once cancels up to the whole bound. Keep the margin
    // loose. Hitting `--max-concurrency` queues; hitting the pool's limit
    // traps the store and loses every request on it.
    let stack_pool = max_concurrency_u32
        .saturating_mul(2)
        .saturating_add(workers_u32.saturating_mul(8));
    let engine = runtime::create_serve_engine(cranelift_opt, max_instances, stack_pool, collector)?;
    let component = Component::new(&engine, &wasm)?;
    let linker = runtime::create_linker(&engine)?;
    // Open preopens once at startup; they are attached to every worker
    // generation's `WasiState`.
    let preopens = Arc::new(Preopens::open(&preopened_dirs)?);
    let instance_pre = linker.instantiate_pre(&component)?;
    let service_pre = Arc::new(ServicePre::<WasiState>::new(instance_pre)?);
    drop(linker);

    // Set up the guest profiler before the component handle is dropped.
    // The profiler is component-scoped; each worker store re-registers the
    // sampling callback (see `worker_loop`).
    let profiler_handle = if let ProfileMode::Guest { interval_ms, .. } = &profile {
        let interval = Duration::from_millis(*interval_ms);
        let profiler = GuestProfiler::new_component(
            &engine,
            "wado",
            interval,
            component.clone(),
            std::iter::empty::<(String, wasmtime::Module)>(),
        )?;
        Some(GuestProfilerHandle {
            profiler: Arc::new(Mutex::new(Some(profiler))),
            interval,
        })
    } else {
        None
    };
    drop(component);

    // One worker = one instance on its own store, so guest execution fans out
    // across cores. Its queue holds its share again, where a burst waits.
    let per_worker_inflight = max_concurrency / workers;
    assert!(
        per_worker_inflight >= 1,
        "--workers is held at or below --max-concurrency, in `parse_args` when \
         both are explicit and in `run` when the worker count is derived",
    );
    // Coarse server-wide clock, replacing a timer per request and per worker.
    // The interval bounds how late the 504 fires, so keep it small relative to
    // `timeout` but coarse enough to be cheap. A tokio task will do: it backs
    // the 504, not the runaway-guest trap, so it may share their scheduling.
    let (tick_tx, tick_rx) = watch::channel(());
    let first_byte_tick = (timeout / 8).clamp(Duration::from_millis(100), Duration::from_secs(1));
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(first_byte_tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            // Stops once every receiver (held via `Dispatch` and the workers)
            // is gone.
            if tick_tx.send(()).is_err() {
                break;
            }
        }
    });
    // Notified by a worker that fails to instantiate; the accept loop
    // treats it as a fatal shutdown (see `worker_loop`).
    let fatal = Arc::new(Notify::new());
    let mut txs = Vec::with_capacity(workers);
    let mut engine_tasks = Vec::with_capacity(workers);
    for _ in 0..workers {
        let (tx, rx) = mpsc::channel::<RequestJob>(per_worker_inflight);
        txs.push(tx);
        engine_tasks.push(tokio::spawn(worker_loop(
            engine.clone(),
            Arc::clone(&service_pre),
            Arc::clone(&preopens),
            rx,
            per_worker_inflight,
            tick_rx.clone(),
            recycle_requests,
            timeout.as_secs().max(1),
            Arc::clone(&fatal),
            profiler_handle.clone(),
        )));
    }
    let dispatch = Arc::new(Dispatch {
        txs,
        next: AtomicUsize::new(0),
        tick_rx,
    });

    // Advances the engine epoch the worker deadlines count in. It needs an OS
    // thread: a guest running away in pure wasm blocks the tokio worker
    // polling it, which on a single-core host is the only one, so a tokio
    // ticker would be starved by the guest it exists to trap.
    let epoch_tick = match &profile {
        ProfileMode::Guest { interval_ms, .. } => Duration::from_millis(*interval_ms),
        _ => Duration::from_millis(EPOCH_TICK_MS),
    };
    let epoch_stop = Arc::new((Mutex::new(false), Condvar::new()));
    let epoch_ticker = {
        let engine = engine.clone();
        let epoch_stop = Arc::clone(&epoch_stop);
        std::thread::spawn(move || {
            let (stop_flag, cvar) = &*epoch_stop;
            let mut stop = lock(stop_flag);
            while !*stop {
                let (next, wait) = cvar
                    .wait_timeout(stop, epoch_tick)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                stop = next;
                if wait.timed_out() {
                    engine.increment_epoch();
                }
            }
        })
    };

    let addr: SocketAddr = addr.parse()?;
    let listener = TcpListener::bind(addr).await?;
    // Report the *bound* address, not the requested one: a request for
    // port 0 is resolved by the kernel to a concrete free port, and
    // callers (e.g. the e2e harness) parse this line to learn it.
    let bound_addr = listener.local_addr()?;

    let recycle_desc = if recycle_requests == 0 {
        "off".to_string()
    } else {
        format!("every {recycle_requests} req")
    };
    eprintln!("HTTP server listening on http://{bound_addr}/");
    eprintln!(
        "Instance reuse: ON — {workers} worker(s), {per_worker_inflight} in flight each, recycle {recycle_desc}"
    );
    eprintln!("Per-request timeout: {}s", timeout.as_secs());
    #[cfg(unix)]
    eprintln!("Send SIGINT or SIGTERM to shut down");
    #[cfg(not(unix))]
    eprintln!("Send Ctrl+C to shut down");

    let mut connections: JoinSet<()> = JoinSet::new();
    // Resolves to `true` when shutdown was triggered by a fatal worker
    // failure, `false` for a normal OS signal.
    let mut shutdown = pin!(async {
        let sig = pin!(shutdown_signal());
        let fat = pin!(fatal.notified());
        matches!(select(sig, fat).await, Either::Right(((), _)))
    });
    let fatal_shutdown;

    // Accept loop with graceful shutdown. On signal, stop accepting and
    // wait for in-flight connections to drain (with a hard cap so we don't
    // hang forever on misbehaving clients).
    loop {
        let accept = pin!(listener.accept());
        match select(accept, shutdown.as_mut()).await {
            Either::Right((is_fatal, _accept)) => {
                fatal_shutdown = is_fatal;
                if is_fatal {
                    eprintln!(
                        "Fatal: a worker failed to instantiate; shutting down ({} in-flight connection(s))",
                        connections.len()
                    );
                } else {
                    eprintln!(
                        "Shutdown signal received; draining {} in-flight connection(s)…",
                        connections.len()
                    );
                }
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
                // Disable Nagle's algorithm. Responses are written head-first
                // and body-second (the body streams in asynchronously from the
                // guest), so with Nagle on the body write can stall waiting for
                // the ACK of the head — a classic latency hit under load.
                if let Err(e) = stream.set_nodelay(true) {
                    eprintln!("warning: failed to set TCP_NODELAY for {remote_addr}: {e}");
                }
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
    {
        let (stop_flag, cvar) = &*epoch_stop;
        *lock(stop_flag) = true;
        cvar.notify_all();
    }
    let _ = epoch_ticker.join();

    // Every worker has stopped, so no further samples can land: finish the
    // guest profile and write it out.
    if let (Some(handle), ProfileMode::Guest { path, .. }) = (profiler_handle, &profile)
        && let Some(profiler) = lock(&handle.profiler).take()
    {
        match std::fs::File::create(path) {
            Ok(file) => match profiler.finish(std::io::BufWriter::new(file)) {
                Ok(()) => {
                    eprintln!("Profile written to {path}");
                    eprintln!("View at https://profiler.firefox.com/");
                }
                Err(e) => eprintln!("Failed to write profile to {path}: {e}"),
            },
            Err(e) => eprintln!("Failed to create profile file {path}: {e}"),
        }
    }

    if fatal_shutdown {
        anyhow::bail!("a worker failed to instantiate the component");
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

pub async fn run(opts: ServeOptions) -> Result<(), CliExit> {
    let cranelift_opt = opts.knobs.opt_level.to_wasmtime();
    let flags = CompileFlags {
        knobs: opts.knobs,
        target_world: Some("wasi:http/service".to_string()),
        ..CompileFlags::default()
    };
    // `serve` is a driver on the build tier: in a project it builds the
    // http/service world through the shared build core (metadata embedded,
    // written to build/), then serves it; a bare file with no project stays on
    // the in-memory compile primitive.
    let wasm = build_for_driver(&opts.input, "wasi:http/service", &flags).await?;

    let timeout = Duration::from_secs(opts.timeout_secs);
    // An explicit `--workers` is already validated against an explicit
    // `--max-concurrency` in `parse_args`; the auto-derived default is
    // clamped so it never sizes more workers than the bound can back.
    let mut workers = opts.workers.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(1)
    });
    if let Some(max_concurrency) = opts.max_concurrency {
        workers = workers.min(max_concurrency);
    }
    // Guest profiling samples one worker store; more than one worker would
    // conflate independent stores into a single profile.
    if matches!(opts.profile, ProfileMode::Guest { .. }) && workers != 1 {
        eprintln!("Profiling: forcing --workers 1");
        workers = 1;
    }
    // Derived after the worker count is final, so each worker gets the
    // per-worker default whatever the host's CPU count turned out to be.
    // Held to the same `u32` as an explicit `--max-concurrency`, which is
    // what `run_http_server` converts it back to.
    let max_concurrency = opts.max_concurrency.unwrap_or_else(|| {
        workers
            .saturating_mul(DEFAULT_MAX_CONCURRENCY_PER_WORKER)
            .min(u32::MAX as usize)
    });
    run_http_server(
        wasm,
        &opts.addr,
        cranelift_opt,
        opts.preopened_dirs,
        timeout,
        workers,
        opts.recycle_requests,
        max_concurrency,
        opts.profile,
        opts.collector,
    )
    .await
    .map_err(|e| CliExit::error(format!("Server error: {e}")))
}
