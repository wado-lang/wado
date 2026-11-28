//! A child that borrows the host's memory until it calls `exec`, as `vfork`
//! and `posix_spawn` have theirs do.
//!
//! `fork` copies the page tables of everything the host holds and leaves each
//! page to fault on its next write. A spawn from a host holding gigabytes then
//! costs in proportion, and slows every thread of the host with it.

use std::ffi::{CStr, CString, OsString, c_void};
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use super::linux::retried;

/// What the child runs on. It makes system calls alone, so a stack far below
/// a thread's is ample.
const STACK_SIZE: usize = 256 * 1024;

const SHELL: &CStr = c"/bin/sh";

/// Run `child` in a new process that shares the host's memory, on a stack of
/// its own, and return its pid once it has called `exec` or exited. What
/// `child` returns is the status the process exits with, and an error it
/// returns is this call's, with the process reaped.
///
/// The process starts with a copy of every descriptor the host holds, and
/// closes all but the standard streams and `keep` before anything else. A file
/// another thread of the host has open for writing is then held open only for
/// that instant, rather than while `child` confines itself, which narrows the
/// window in which that thread's `exec` of the file fails with `ETXTBSY`.
///
/// # Safety
///
/// `child` runs beside the host's other threads in their memory, so it must
/// make only system calls on memory built before the call: it takes no lock,
/// allocates nothing and does not panic.
pub(super) unsafe fn spawn(
    keep: &[RawFd],
    mut child: impl FnMut() -> io::Result<libc::c_int>,
) -> io::Result<libc::pid_t> {
    assert!(
        keep.is_sorted_by(|a, b| a < b) && keep.first().is_none_or(|&fd| fd > libc::STDERR_FILENO),
        "kept descriptors must ascend past the standard streams: {keep:?}"
    );
    let mut entry = Entry {
        keep,
        child: &mut child,
        failure: None,
    };
    let stack = Stack::new()?;
    // SAFETY: plain calls on signal sets that live on this stack.
    let old = unsafe {
        let mut all: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&raw mut all);
        // No handler of the host's may run in the child, which shares the
        // memory the handler would write.
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const all, &raw mut old);
        old
    };
    // SAFETY: `enter` reads `entry` through the pointer while this frame
    // waits, `CLONE_VFORK` holding it until the child is done with the memory.
    let pid = unsafe {
        libc::clone(
            enter,
            stack.top(),
            libc::CLONE_VM | libc::CLONE_VFORK | libc::SIGCHLD,
            (&raw mut entry).cast::<c_void>(),
        )
    };
    let cloned = if pid == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(pid)
    };
    // SAFETY: `old` is the mask the call above returned.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, ptr::null_mut()) };
    let pid = cloned?;
    if let Some(failure) = entry.failure {
        reap(pid)?;
        return Err(failure);
    }
    Ok(pid)
}

/// The status the child `pid` ended with, once it has.
pub(super) fn reap(pid: libc::pid_t) -> io::Result<libc::c_int> {
    let mut status = 0;
    // SAFETY: `status` outlives the call.
    retried(|| unsafe { libc::waitpid(pid, &raw mut status, 0) })?;
    Ok(status)
}

/// What [`spawn`] hands the new process, and what it hands back.
struct Entry<'a> {
    keep: &'a [RawFd],
    child: &'a mut dyn FnMut() -> io::Result<libc::c_int>,
    failure: Option<io::Error>,
}

extern "C" fn enter(entry: *mut c_void) -> libc::c_int {
    // SAFETY: `spawn` passes a pointer to its `Entry`, which it keeps alive
    // and does not touch until this process is done with it.
    let entry = unsafe { &mut *entry.cast::<Entry<'_>>() };
    let ran = close_all_but(entry.keep).and_then(|()| {
        default_caught_signals();
        (entry.child)()
    });
    ran.unwrap_or_else(|err| {
        entry.failure = Some(err);
        127
    })
}

/// Close every descriptor past the standard streams that `keep`, ascending,
/// does not list.
fn close_all_but(keep: &[RawFd]) -> io::Result<()> {
    let close = |first: u32, last: u32| {
        // SAFETY: a plain system call.
        if first <= last && unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    };
    let mut first = libc::STDERR_FILENO.cast_unsigned() + 1;
    for &fd in keep {
        let fd = fd.cast_unsigned();
        close(first, fd - 1)?;
        first = fd + 1;
    }
    close(first, u32::MAX)
}

/// Every signal the host catches is back at its default, so the child may
/// unblock signals before its `exec` without running a handler of the host's.
/// The table of handlers is the child's own copy.
fn default_caught_signals() {
    for signal in 1..=libc::SIGRTMAX() {
        // SAFETY: plain calls on an action that lives on this stack. One the
        // kernel refuses, as for `SIGKILL`, is left as it is.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, ptr::null(), &raw mut action) == 0
                && action.sa_sigaction != libc::SIG_DFL
                && action.sa_sigaction != libc::SIG_IGN
            {
                action.sa_sigaction = libc::SIG_DFL;
                libc::sigaction(signal, &raw const action, ptr::null_mut());
            }
        }
    }
}

/// What `exec` takes, in the form it takes it, built where allocating is safe.
pub(super) struct Image {
    program: CString,
    /// Each ends in a null, and points into `_strings`. `args` opens with the
    /// shell and then the program, for a program the shell has to run.
    args: Vec<*const libc::c_char>,
    env: Vec<*const libc::c_char>,
    _strings: Vec<CString>,
}

impl Image {
    /// `program` with `args` after its own name, in an environment of `env`
    /// alone.
    pub(super) fn new(
        program: &Path,
        args: &[OsString],
        env: &[(OsString, OsString)],
    ) -> io::Result<Self> {
        let program = CString::new(program.as_os_str().as_bytes())?;
        let mut strings = Vec::with_capacity(args.len() + env.len());
        for arg in args {
            strings.push(CString::new(arg.as_bytes())?);
        }
        for (name, value) in env {
            strings.push(CString::new(
                [name.as_bytes(), b"=", value.as_bytes()].concat(),
            )?);
        }
        let (arg_strings, env_strings) = strings.split_at(args.len());
        let ended = |list: &[CString]| -> Vec<*const libc::c_char> {
            list.iter()
                .map(|string| string.as_ptr())
                .chain([ptr::null()])
                .collect()
        };
        let mut args = vec![SHELL.as_ptr(), program.as_ptr()];
        args.extend(ended(arg_strings));
        Ok(Self {
            args,
            env: ended(env_strings),
            program,
            _strings: strings,
        })
    }
}

/// Replace the child with `image`, on `streams` as its standard streams where
/// one is given, and return why it could not. For a child of [`spawn`].
pub(super) fn exec(image: &Image, streams: [Option<RawFd>; 3]) -> io::Error {
    let Image {
        program, args, env, ..
    } = image;
    // SAFETY: plain system calls, on lists `Image::new` ended in a null.
    unsafe {
        for (target, stream) in (0..).zip(streams) {
            if let Some(stream) = stream
                && libc::dup2(stream, target) == -1
            {
                return io::Error::last_os_error();
            }
        }
        // The state a program expects to start in: the host ignores
        // `SIGPIPE`, and `spawn` blocked every signal.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut none);
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const none, ptr::null_mut());
        libc::execve(program.as_ptr(), args[1..].as_ptr(), env.as_ptr());
        // An executable file in no format the kernel knows is a script that
        // names no interpreter, which `execvp` and a shell hand to `sh`.
        if io::Error::last_os_error().raw_os_error() == Some(libc::ENOEXEC) {
            libc::execve(SHELL.as_ptr(), args.as_ptr(), env.as_ptr());
        }
    }
    io::Error::last_os_error()
}

/// A mapping for the child's stack, with a guard page below it.
struct Stack {
    start: *mut c_void,
    len: usize,
}

impl Stack {
    fn new() -> io::Result<Self> {
        // SAFETY: plain system call.
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .expect("a page has a size");
        let len = STACK_SIZE + page;
        // SAFETY: a fresh anonymous mapping, ours alone.
        let start = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
                -1,
                0,
            )
        };
        if start == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let stack = Self { start, len };
        // SAFETY: the first page of the mapping above.
        if unsafe { libc::mprotect(start, page, libc::PROT_NONE) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(stack)
    }

    /// Where a stack that grows down starts. `mmap` aligns both ends to a
    /// page, which is more than a stack pointer asks.
    fn top(&self) -> *mut c_void {
        // SAFETY: one past the end of the mapping.
        unsafe { self.start.byte_add(self.len) }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, which the child is done with.
        let unmapped = unsafe { libc::munmap(self.start, self.len) };
        assert_eq!(unmapped, 0, "munmap: {}", io::Error::last_os_error());
    }
}
