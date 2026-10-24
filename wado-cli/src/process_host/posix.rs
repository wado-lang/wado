//! Spawning on a POSIX host, where `wado_host::sandbox` confines the child.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::panic::resume_unwind;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::task::{Context, Poll};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::unix::pipe;
use tokio::sync::oneshot;
use wasmtime::component::{
    Access, Destination, FutureReader, Resource, Source, StreamConsumer, StreamProducer,
    StreamReader, StreamResult,
};
use wasmtime::{AsContextMut, StoreContextMut};
use wasmtime_wasi::filesystem::Descriptor;
use wasmtime_wasi::p2::OutputStream;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

use wado_host::sandbox::{self, Access as Reach, Child, Network, Policy, Stdio};

use super::types::{ExitStatus, Input, Output};
use super::{Command, ErrorCode, GuestStdio, ProcessHost, ProcessView, Spawned};
use crate::external::find_program;

/// How long a child has between `SIGTERM` at its timeout and `SIGKILL`.
const TIMEOUT_GRACE: Duration = Duration::from_secs(3);

const PIPE_BUFFER: usize = 8192;

/// A child the guest holds. Dropping it, by the guest or with the store as the
/// guest exits, kills the child's process group.
pub struct ChildHandle(Arc<Child>);

impl ChildHandle {
    pub(super) fn kill(&self) {
        self.0
            .signal_group(libc::SIGKILL)
            .expect("a confined child cannot leave the group its host signals");
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Where the host routes one of the child's output streams.
enum Route {
    Null,
    Inherit,
    /// To the guest, as a stream.
    Piped,
    /// Into the guest's captured stream, since the child cannot write there.
    Captured(MemoryOutputPipe),
}

impl Route {
    fn stdio(&self) -> Stdio {
        match self {
            Route::Null => Stdio::Null,
            Route::Inherit => Stdio::Inherit,
            Route::Piped | Route::Captured(_) => Stdio::Piped,
        }
    }
}

struct Started {
    spawned: sandbox::Spawned,
    stdout: Route,
    stderr: Route,
    scratch: TempDir,
}

pub(super) fn spawn<U>(
    mut store: Access<'_, U, ProcessHost>,
    cmd: Command,
    cwd: Option<Resource<Descriptor>>,
) -> wasmtime::Result<Result<Spawned, ErrorCode>> {
    let Command {
        program,
        args,
        env,
        stdin,
        stdout,
        stderr,
        timeout,
    } = cmd;
    let started = start(
        store.get(),
        &program,
        &args,
        &env,
        &stdin,
        [stdout, stderr],
        cwd.as_ref(),
    )?;
    let Started {
        spawned,
        stdout,
        stderr,
        scratch,
    } = match started {
        Ok(started) => started,
        Err(code) => {
            if let Input::Piped(mut unread) = stdin {
                unread.close(&mut store)?;
            }
            return Ok(Err(code));
        }
    };
    let child = Arc::new(spawned.child);
    let handle = store.get().table.push(ChildHandle(Arc::clone(&child)))?;
    if let Input::Piped(data) = stdin {
        let fd = spawned.stdin.expect("a piped stdin has a host end");
        data.pipe(&mut store, PipeConsumer(pipe::Sender::from_owned_fd(fd)?))?;
    }
    let mut copies = Vec::new();
    let stdout = route(&mut store, stdout, spawned.stdout, &mut copies)?;
    let stderr = route(&mut store, stderr, spawned.stderr, &mut copies)?;
    let exited = supervise(child, timeout.map(Duration::from_nanos), scratch, copies)?;
    let exit = FutureReader::new(&mut store, async move {
        wasmtime::error::Ok(exited.await.expect("the waiter reports every child"))
    })?;
    Ok(Ok(Spawned {
        child: handle,
        stdout,
        stderr,
        exit,
    }))
}

/// Everything up to the child running, which fails with an `ErrorCode` alone.
fn start(
    view: ProcessView<'_>,
    program: &str,
    args: &[String],
    env: &[(String, String)],
    stdin: &Input,
    [stdout, stderr]: [Output; 2],
    cwd: Option<&Resource<Descriptor>>,
) -> wasmtime::Result<Result<Started, ErrorCode>> {
    let grants = view.ctx.grants()?;
    let cwd = match cwd {
        Some(cwd) => match view.table.get(cwd)? {
            Descriptor::Dir(dir) => Some(Arc::clone(&dir.dir)),
            Descriptor::File(_) => return Ok(Err(ErrorCode::Io)),
        },
        None => grants.first().map(|grant| Arc::clone(&grant.dir)),
    };
    let host_stdio = matches!(view.ctx.stdio, GuestStdio::Host);
    let stdin = match stdin {
        Input::Null => Stdio::Null,
        Input::Inherit if host_stdio => Stdio::Inherit,
        Input::Inherit => Stdio::Null,
        Input::Piped(_) => Stdio::Piped,
    };
    let [stdout, stderr] = [(stdout, 0), (stderr, 1)].map(|(output, index)| match output {
        Output::Null => Route::Null,
        Output::Piped => Route::Piped,
        Output::Inherit => match &view.ctx.stdio {
            GuestStdio::Host => Route::Inherit,
            GuestStdio::Captured { stdout, stderr } => {
                Route::Captured([stdout, stderr][index].clone())
            }
        },
    });
    let inherits = stdin == Stdio::Inherit
        || matches!(stdout, Route::Inherit)
        || matches!(stderr, Route::Inherit);
    let launched = (|| {
        let scratch = Scratch::new()?;
        let cwd = match cwd {
            Some(cwd) => cwd,
            None => Arc::new(File::open(scratch.home())?),
        };
        let program = resolve(program, &cwd)?;
        let mut policy = Policy::new(Network::Granted);
        for grant in grants {
            policy.grant_dir(grant.path.clone(), Arc::clone(&grant.dir));
        }
        policy.grant_path(scratch.0.path(), Reach::ReadWrite);
        if inherits {
            policy.grant_terminal();
        }
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let spawned = sandbox::spawn(&sandbox::Command {
            program: &program,
            args: &args,
            env: &child_env(env, &scratch, inherits),
            cwd: Some(&cwd),
            stdin,
            stdout: stdout.stdio(),
            stderr: stderr.stdio(),
            policy: &policy,
        })?;
        io::Result::Ok((spawned, scratch))
    })();
    Ok(match launched {
        Ok((spawned, scratch)) => Ok(Started {
            spawned,
            stdout,
            stderr,
            scratch: scratch.0,
        }),
        Err(err) => Err(error_code(&err)),
    })
}

/// A child's private `HOME` and `TMPDIR`, removed once it exits.
struct Scratch(TempDir);

impl Scratch {
    fn new() -> io::Result<Self> {
        let scratch = Self(tempfile::Builder::new().prefix("wado-process-").tempdir()?);
        fs::create_dir(scratch.home())?;
        fs::create_dir(scratch.tmp())?;
        Ok(scratch)
    }

    fn home(&self) -> PathBuf {
        self.0.path().join("home")
    }

    fn tmp(&self) -> PathBuf {
        self.0.path().join("tmp")
    }
}

/// A path when it holds a `/`, relative to `cwd`; otherwise a name on `PATH`.
fn resolve(program: &str, cwd: &File) -> io::Result<PathBuf> {
    if program.contains('/') {
        let path = Path::new(program);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        return Ok(sandbox::descriptor_path(cwd)?.join(path));
    }
    find_program(program).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
}

/// The baseline variables, then the guest's, which override them.
fn child_env(
    env: &[(String, String)],
    scratch: &Scratch,
    inherits: bool,
) -> Vec<(OsString, OsString)> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            key == "PATH" || key == "LANG" || key.starts_with("LC_") || (inherits && key == "TERM")
        })
        .collect();
    vars.push(("HOME".into(), scratch.home().into()));
    vars.push(("TMPDIR".into(), scratch.tmp().into()));
    vars.retain(|(key, _)| !env.iter().any(|(guest, _)| key == guest.as_str()));
    vars.extend(env.iter().map(|(key, value)| (key.into(), value.into())));
    vars
}

fn error_code(err: &io::Error) -> ErrorCode {
    match err.raw_os_error() {
        Some(libc::ENOENT | libc::ENOTDIR) => ErrorCode::NotFound,
        Some(libc::EACCES | libc::EPERM) => ErrorCode::NotPermitted,
        Some(libc::EAGAIN | libc::ENOMEM | libc::EMFILE | libc::ENFILE) => {
            ErrorCode::ResourceExhausted
        }
        _ if err.kind() == io::ErrorKind::NotFound => ErrorCode::NotFound,
        _ => ErrorCode::Io,
    }
}

/// The guest's end of one output stream, where it gets one. A captured stream
/// is copied on a thread pushed to `copies`.
fn route<T: 'static>(
    mut store: impl AsContextMut<Data = T>,
    route: Route,
    fd: Option<OwnedFd>,
    copies: &mut Vec<JoinHandle<()>>,
) -> wasmtime::Result<Option<StreamReader<u8>>> {
    match route {
        Route::Null | Route::Inherit => Ok(None),
        Route::Piped => {
            let fd = fd.expect("a piped stream has a host end");
            let producer = PipeProducer(pipe::Receiver::from_owned_fd(fd)?);
            Ok(Some(StreamReader::new(&mut store, producer)?))
        }
        Route::Captured(sink) => {
            copies.push(copy_into(
                fd.expect("a captured stream has a host end"),
                sink,
            )?);
            Ok(None)
        }
    }
}

/// Copy a child's stream into a captured one until the child closes it.
fn copy_into(fd: OwnedFd, mut sink: MemoryOutputPipe) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("process-capture".to_owned())
        .spawn(move || {
            let mut source = File::from(fd);
            let mut buf = vec![0; PIPE_BUFFER];
            loop {
                match source.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        // A full capture refuses more; dropping the source
                        // breaks the child's pipe.
                        if sink.write(Bytes::copy_from_slice(&buf[..n])).is_err() {
                            return;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        })
}

/// Wait for `child` on a thread of its own, enforcing `timeout`, and remove
/// its scratch directory once it exits. The exit is reported once `copies`
/// have drained what the child wrote, so the guest never sees it first.
fn supervise(
    child: Arc<Child>,
    timeout: Option<Duration>,
    scratch: TempDir,
    copies: Vec<JoinHandle<()>>,
) -> io::Result<oneshot::Receiver<Result<ExitStatus, ErrorCode>>> {
    let (report, exited) = oneshot::channel();
    thread::Builder::new()
        .name("process-wait".to_owned())
        .spawn(move || {
            let (alive, deadline) = mpsc::channel::<()>();
            let timer = timeout.map(|timeout| {
                let child = Arc::clone(&child);
                thread::spawn(move || enforce(&child, &deadline, timeout))
            });
            let waited = child.wait();
            drop(alive);
            let timed_out =
                timer.is_some_and(|timer| timer.join().unwrap_or_else(|p| resume_unwind(p)));
            drop(scratch);
            for copy in copies {
                copy.join().unwrap_or_else(|p| resume_unwind(p));
            }
            let status = match waited {
                Err(_) => Err(ErrorCode::Io),
                Ok(_) if timed_out => Ok(ExitStatus::TimedOut),
                Ok(sandbox::ExitStatus::Exited(code)) => Ok(ExitStatus::Exited(code)),
                Ok(sandbox::ExitStatus::Signaled(signal)) => Ok(ExitStatus::Signaled(signal)),
            };
            // The guest may have dropped the future without waiting.
            let _ = report.send(status);
        })?;
    Ok(exited)
}

/// End the child's group at `timeout`: `SIGTERM`, then `SIGKILL` after the
/// grace. Returns whether the timeout ended it; `alive` disconnects at exit.
fn enforce(child: &Child, alive: &Receiver<()>, timeout: Duration) -> bool {
    if alive.recv_timeout(timeout) != Err(RecvTimeoutError::Timeout) {
        return false;
    }
    let signal = |signal| {
        child
            .signal_group(signal)
            .expect("a confined child cannot leave the group its host signals");
    };
    signal(libc::SIGTERM);
    if alive.recv_timeout(TIMEOUT_GRACE) == Err(RecvTimeoutError::Timeout) {
        signal(libc::SIGKILL);
    }
    true
}

/// A child's output, read as the guest asks.
struct PipeProducer(pipe::Receiver);

impl<D> StreamProducer<D> for PipeProducer {
    type Item = u8;
    type Buffer = BytesMut;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'a, D>,
        dst: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        // A zero-length read asks whether data is ready, which `AsyncRead`
        // cannot answer (WebAssembly/component-model#561), so it says yes.
        if dst.remaining(store.as_context_mut()) == Some(0) {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let mut dst = dst.as_direct(store, PIPE_BUFFER);
        let mut buf = ReadBuf::new(dst.remaining());
        match Pin::new(&mut self.0).poll_read(cx, &mut buf) {
            Poll::Ready(Ok(())) if buf.filled().is_empty() => {
                Poll::Ready(Ok(StreamResult::Dropped))
            }
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len();
                dst.mark_written(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Ready(Err(_)) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A child's input, written as the guest sends it.
struct PipeConsumer(pipe::Sender);

impl<D> StreamConsumer<D> for PipeConsumer {
    type Item = u8;

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        store: StoreContextMut<D>,
        src: Source<Self::Item>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut src = src.as_direct(store);
        let buf = src.remaining();
        // As in `PipeProducer`: an empty write asks whether the pipe is ready.
        if buf.is_empty() {
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        match Pin::new(&mut self.0).poll_write(cx, buf) {
            Poll::Ready(Ok(0) | Err(_)) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Ready(Ok(n)) => {
                src.mark_read(n);
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending if finish => Poll::Ready(Ok(StreamResult::Cancelled)),
            Poll::Pending => Poll::Pending,
        }
    }
}
