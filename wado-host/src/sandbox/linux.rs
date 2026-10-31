//! Linux confinement. Landlock restricts the filesystem, signals and abstract
//! UNIX sockets; seccomp covers what Landlock leaves: the network, UNIX sockets
//! named by a path, leaving the process group, typing into a terminal,
//! `ptrace`, and `io_uring`, whose operations seccomp never sees.
//!
//! Everything the child reads is built before the fork, so the window between
//! `fork` and `exec` makes only system calls.
//!
//! The filter refers each call it refuses to the host through a seccomp
//! listener, which the child passes back over a socket pair before its `exec`.
//! A thread records the call as a [`Refusal`] and answers `EPERM`, so the child
//! sees what a plain `SECCOMP_RET_ERRNO` would have given it. Under
//! [`Trace::Files`] the filter also refers each call Landlock mediates; the
//! thread checks its paths against the policy and lets it continue, so Landlock
//! still decides.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio as StdStdio};
use std::ptr;
use std::sync::OnceLock;

use super::{
    Access, Availability, Child, Command, Network, Policy, Refusal, Refusals, ResolvedPolicy,
    Spawned, Stdio, Terminal, Trace, give_terminal,
};
use crate::sync::spawn_detached;

/// ABI 6 adds the scopes that isolate signals and abstract UNIX sockets.
const LANDLOCK_ABI_REQUIRED: libc::c_long = 6;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;
/// What `Access::ReadExecute` grants.
const ACCESS_FS_READ: u64 = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
/// Every filesystem right ABI 6 handles, `EXECUTE` through `IOCTL_DEV`.
const ACCESS_FS_ALL: u64 = (1 << 16) - 1;
/// The rights a rule on a file, rather than a directory, may hold.
const ACCESS_FS_FILE: u64 = ACCESS_FS_EXECUTE
    | ACCESS_FS_WRITE_FILE
    | ACCESS_FS_READ_FILE
    | ACCESS_FS_TRUNCATE
    | ACCESS_FS_IOCTL_DEV;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The `AUDIT_ARCH_*` value a seccomp filter checks, for the architectures it
/// is written for.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: Option<u32> = Some(0xC000_003E);
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: Option<u32> = Some(0xC000_00B7);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const AUDIT_ARCH: Option<u32> = None;

/// The bit that marks an x32 system call, which shares the x86-64 audit arch.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: Option<u32> = Some(0x4000_0000);
#[cfg(not(target_arch = "x86_64"))]
const X32_SYSCALL_BIT: Option<u32> = None;

pub(super) fn availability() -> Availability {
    static ANSWER: OnceLock<Availability> = OnceLock::new();
    ANSWER.get_or_init(probe_availability).clone()
}

fn probe_availability() -> Availability {
    if AUDIT_ARCH.is_none() {
        return Availability::Unsupported(format!(
            "no seccomp filter is written for {}",
            std::env::consts::ARCH
        ));
    }
    // SAFETY: the version query reads no memory.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi < 0 {
        return Availability::Unsupported(format!(
            "Landlock is unavailable: {}",
            io::Error::last_os_error()
        ));
    }
    if abi < LANDLOCK_ABI_REQUIRED {
        return Availability::Unsupported(format!(
            "the kernel offers Landlock ABI {abi}; confining a child needs ABI {LANDLOCK_ABI_REQUIRED} (Linux 6.12)"
        ));
    }
    Availability::Available
}

/// Whether a child may take a seccomp listener. The kernel allows one per
/// chain of filters, so under an enclosing sandbox that holds one (LXD's
/// system call interception, Sysbox) installing ours fails with `EBUSY`.
fn listens() -> io::Result<bool> {
    static ANSWER: OnceLock<bool> = OnceLock::new();
    if let Some(&answer) = ANSWER.get() {
        return Ok(answer);
    }
    let answer = probe_listener()?;
    Ok(*ANSWER.get_or_init(|| answer))
}

fn probe_listener() -> io::Result<bool> {
    let mut asm = Assembler::default();
    asm.ret(libc::SECCOMP_RET_ALLOW);
    let filter = asm.finish();
    // SAFETY: the child makes only system calls, on memory built before the
    // fork, and exits without returning.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: as above.
        let listens = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0
            && install_filter(&filter, libc::SECCOMP_FILTER_FLAG_NEW_LISTENER).is_ok();
        // SAFETY: as above.
        unsafe { libc::_exit(i32::from(!listens)) };
    }
    check(pid)?;
    let mut status = 0;
    // SAFETY: `status` outlives the call.
    retried(|| unsafe { libc::waitpid(pid, &raw mut status, 0) })?;
    Ok(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0)
}

pub(super) fn spawn(command: &Command<'_>, terminal: Terminal) -> io::Result<Spawned> {
    let ruleset = ruleset(command.policy)?;
    let listens = listens()?;
    let (refer, trace) = if listens {
        (libc::SECCOMP_RET_USER_NOTIF, command.trace)
    } else {
        (libc::SECCOMP_RET_ERRNO | libc::EPERM as u32, Trace::Off)
    };
    let filter = seccomp_filter(command.policy.network(), trace, refer);
    let traced = match trace {
        Trace::Off => None,
        Trace::Files => Some(command.policy.resolved()),
    };
    let (listener_tx, listener_rx) = match listens.then(socket_pair).transpose()? {
        Some([tx, rx]) => (Some(tx), Some(rx)),
        None => (None, None),
    };
    let cwd = command.cwd.map(File::try_clone).transpose()?;

    let mut std_command = StdCommand::new(command.program);
    std_command
        .args(command.args)
        .env_clear()
        .envs(command.env.iter().map(|(key, value)| (key, value)))
        .stdin(std_stdio(command.stdin))
        .stdout(std_stdio(command.stdout))
        .stderr(std_stdio(command.stderr));
    // SAFETY: the closure makes only system calls, on memory built before the
    // fork, so it is safe in the child of a multithreaded process.
    unsafe {
        std_command.pre_exec(move || {
            confine_self(
                &ruleset,
                &filter,
                cwd.as_ref(),
                listener_tx.as_ref().map(AsRawFd::as_raw_fd),
                terminal,
            )
        });
    }
    let refusals = Refusals::default();
    if let Some(listener_rx) = listener_rx {
        let recorded = refusals.clone();
        // Started first: the child waits on it before its `exec`. Detached,
        // since it serves the child's descendants, which may outlive it.
        spawn_detached("sandbox-refusals", move || {
            match receive_listener(&listener_rx) {
                Received::Listener(listener, pid) => {
                    reply(&listener_rx, 0);
                    let traced = traced.map(|mut policy| {
                        policy.grant_own_proc(pid);
                        policy
                    });
                    answer_refusals(&listener, &recorded, traced.as_ref());
                }
                // The spawn fails for the reason the host lost it.
                Received::Lost => reply(&listener_rx, libc::EMFILE),
                Received::Nothing => {}
            }
        })?;
    }
    let spawned = std_command.spawn();
    // Closes the host's copy of the sender, which the closure holds.
    drop(std_command);
    let mut child = spawned?;
    Ok(Spawned {
        child: Child::new(child.id() as libc::pid_t, terminal),
        stdin: child.stdin.take().map(OwnedFd::from),
        stdout: child.stdout.take().map(OwnedFd::from),
        stderr: child.stderr.take().map(OwnedFd::from),
        refusals,
        unobserved: (!listens)
            .then_some("an enclosing sandbox holds the one seccomp listener the kernel allows"),
    })
}

fn socket_pair() -> io::Result<[OwnedFd; 2]> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    check(unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    })?;
    // SAFETY: the kernel just returned these descriptors to us alone.
    Ok(fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// The control data of a message carrying one descriptor.
#[repr(C)]
union FdControl {
    header: libc::cmsghdr,
    // SAFETY: `CMSG_SPACE` only computes a size.
    bytes: [u8; unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as u32) } as usize],
}

/// The child's pid with room for one descriptor, all on the stack, since the
/// child builds one between `fork` and `exec`.
struct FdMessage {
    pid: libc::pid_t,
    iov: libc::iovec,
    control: FdControl,
    header: libc::msghdr,
}

impl FdMessage {
    fn new() -> Self {
        // SAFETY: all-zero is a valid value of each field.
        unsafe { std::mem::zeroed() }
    }

    /// The header, pointing into `self`, which must not move while it is used.
    fn header(&mut self) -> *mut libc::msghdr {
        self.iov = libc::iovec {
            iov_base: (&raw mut self.pid).cast(),
            iov_len: size_of::<libc::pid_t>(),
        };
        self.header.msg_iov = &raw mut self.iov;
        self.header.msg_iovlen = 1;
        self.header.msg_control = (&raw mut self.control).cast();
        self.header.msg_controllen = size_of::<FdControl>() as _;
        &raw mut self.header
    }
}

/// Runs in the child between `fork` and `exec`, and returns once the host
/// holds the listener, so no call the filter refers goes unanswered: the
/// error is the host's, where it could not take it.
fn send_listener(socket: RawFd, listener: RawFd) -> io::Result<()> {
    let mut message = FdMessage::new();
    // SAFETY: a plain system call.
    message.pid = unsafe { libc::getpid() };
    let header = message.header();
    // SAFETY: the control buffer holds one header and one descriptor.
    unsafe {
        let control = libc::CMSG_FIRSTHDR(header);
        (*control).cmsg_level = libc::SOL_SOCKET;
        (*control).cmsg_type = libc::SCM_RIGHTS;
        (*control).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        libc::CMSG_DATA(control)
            .cast::<RawFd>()
            .write_unaligned(listener);
    }
    // SAFETY: `message` outlives the call.
    retried(|| unsafe { libc::sendmsg(socket, header, 0) })?;
    let mut errno = 0u8;
    // SAFETY: reads one byte into `errno`, which outlives the call.
    match retried(|| unsafe { libc::recv(socket, (&raw mut errno).cast(), 1, 0) })? {
        0 => Err(io::ErrorKind::BrokenPipe.into()),
        _ if errno == 0 => Ok(()),
        _ => Err(io::Error::from_raw_os_error(i32::from(errno))),
    }
}

/// `call` until a signal no longer interrupts it. Safe between `fork` and
/// `exec`.
fn retried<T: Copy + PartialEq + From<i8>>(mut call: impl FnMut() -> T) -> io::Result<T> {
    loop {
        let ret = call();
        if ret != T::from(-1) {
            return Ok(ret);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Answer the child waiting in [`send_listener`]: 0 to go on, or the error
/// its spawn fails with.
fn reply(socket: &OwnedFd, errno: libc::c_int) {
    let errno = u8::try_from(errno).expect("an errno fits in a byte");
    // SAFETY: sends the one byte `errno` holds.
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            (&raw const errno).cast(),
            1,
            libc::MSG_NOSIGNAL,
        )
    };
    if sent == -1 {
        let err = io::Error::last_os_error();
        // The child died while it waited.
        assert_eq!(err.raw_os_error(), Some(libc::EPIPE), "send: {err}");
    }
}

/// What the child sent over the socket pair.
enum Received {
    Listener(OwnedFd, libc::pid_t),
    /// The child sent its listener, which a host out of descriptors could not
    /// take.
    Lost,
    /// Every sender closed: the spawn failed before the child confined itself.
    Nothing,
}

fn receive_listener(socket: &OwnedFd) -> Received {
    let mut message = FdMessage::new();
    let header = message.header();
    // SAFETY: `message` outlives the call.
    let received =
        retried(|| unsafe { libc::recvmsg(socket.as_raw_fd(), header, libc::MSG_CMSG_CLOEXEC) })
            .unwrap_or_else(|err| panic!("recvmsg: {err}"));
    if received == 0 {
        return Received::Nothing;
    }
    // SAFETY: the kernel filled the control buffer `header` points to.
    let control = unsafe { libc::CMSG_FIRSTHDR(header) };
    // SAFETY: `header` points into `message`, which the kernel wrote.
    if control.is_null() || unsafe { (*header).msg_flags } & libc::MSG_CTRUNC != 0 {
        return Received::Lost;
    }
    // SAFETY: a header the kernel wrote, holding the one descriptor sent, which
    // `SCM_RIGHTS` installed in our table for us alone.
    let listener =
        unsafe { OwnedFd::from_raw_fd(libc::CMSG_DATA(control).cast::<RawFd>().read_unaligned()) };
    Received::Listener(listener, message.pid)
}

/// Answer each call the filter refers to the host, until no task uses the
/// filter any more. A refused call is recorded and answered `EPERM`. A file
/// call, referred only under `traced`, is let through for Landlock to judge,
/// its paths recorded where `traced` grants them not.
fn answer_refusals(listener: &OwnedFd, refusals: &Refusals, traced: Option<&ResolvedPolicy>) {
    let sizes = notif_sizes();
    let mut call = notif_buffer(sizes.seccomp_notif, size_of::<libc::seccomp_notif>());
    let mut answer = notif_buffer(
        sizes.seccomp_notif_resp,
        size_of::<libc::seccomp_notif_resp>(),
    );
    loop {
        let mut ready = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `ready` outlives the call.
        retried(|| unsafe { libc::poll(&raw mut ready, 1, -1) })
            .unwrap_or_else(|err| panic!("poll: {err}"));
        // `POLLHUP` alone: the last task using the filter is gone.
        if ready.revents & libc::POLLIN == 0 {
            return;
        }
        // The kernel refuses a buffer that is not zeroed.
        call.fill(0);
        // SAFETY: `call` is at least the size the kernel writes.
        if unsafe {
            libc::ioctl(
                listener.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_RECV,
                call.as_mut_ptr(),
            )
        } == -1
        {
            match io::Error::last_os_error().raw_os_error() {
                // Interrupted, or the caller died before we read its call.
                Some(libc::EINTR | libc::ENOENT) => continue,
                errno => panic!("SECCOMP_IOCTL_NOTIF_RECV: {errno:?}"),
            }
        }
        // SAFETY: the kernel wrote a `seccomp_notif` at the start of `call`.
        let notif = unsafe { call.as_ptr().cast::<libc::seccomp_notif>().read() };
        let reply = if let Some(paths) = file_call(libc::c_long::from(notif.data.nr)) {
            let policy = traced.expect("the filter refers a file call only when traced");
            match refused_paths(listener, &notif, paths, policy) {
                Ok(refused) => refused
                    .into_iter()
                    .for_each(|refusal| refusals.record(refusal)),
                Err(Unseen) => refusals.record_unseen(),
            }
            libc::seccomp_notif_resp {
                id: notif.id,
                val: 0,
                error: 0,
                flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
            }
        } else {
            refusals.record(refusal(&notif.data));
            libc::seccomp_notif_resp {
                id: notif.id,
                val: 0,
                error: -libc::EPERM,
                flags: 0,
            }
        };
        answer.fill(0);
        // SAFETY: `answer` is at least the size of a `seccomp_notif_resp`.
        unsafe {
            answer
                .as_mut_ptr()
                .cast::<libc::seccomp_notif_resp>()
                .write(reply);
        }
        // SAFETY: as above, `answer` is large enough.
        if unsafe {
            libc::ioctl(
                listener.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_SEND,
                answer.as_ptr(),
            )
        } == -1
        {
            let err = io::Error::last_os_error();
            // The caller died while it waited.
            assert_eq!(
                err.raw_os_error(),
                Some(libc::ENOENT),
                "SECCOMP_IOCTL_NOTIF_SEND: {err}"
            );
        }
    }
}

/// The sizes of the notification structures the running kernel reads and
/// writes, which may exceed the ones `libc` declares.
fn notif_sizes() -> libc::seccomp_notif_sizes {
    // SAFETY: an all-zero `seccomp_notif_sizes` is valid.
    let mut sizes: libc::seccomp_notif_sizes = unsafe { std::mem::zeroed() };
    // SAFETY: `sizes` outlives the call, which writes it.
    check_long(unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_GET_NOTIF_SIZES,
            0u32,
            &raw mut sizes,
        )
    })
    .expect("a kernel that installed the listener reports its sizes");
    sizes
}

/// A zeroed, 8-byte aligned buffer of at least `kernel` and `declared` bytes.
fn notif_buffer(kernel: u16, declared: usize) -> Vec<u64> {
    vec![0; usize::from(kernel).max(declared).div_ceil(8)]
}

/// What the filter refers to the host, by the call and the argument it tested.
fn refusal(call: &libc::seccomp_data) -> Refusal {
    match libc::c_long::from(call.nr) {
        libc::SYS_socket if call.args[0] as libc::c_int == libc::AF_UNIX => Refusal::UnixSocket,
        libc::SYS_socket => Refusal::Network,
        libc::SYS_setsid | libc::SYS_setpgid => Refusal::LeaveGroup,
        libc::SYS_ptrace => Refusal::Trace,
        libc::SYS_io_uring_setup => Refusal::IoUring,
        libc::SYS_ioctl => Refusal::TerminalInput,
        nr => unreachable!("the filter refers no other call: {nr}"),
    }
}

/// Where a file call takes a path: alone, relative to the working directory,
/// or after the directory descriptor it is relative to.
#[derive(Clone, Copy)]
enum PathArg {
    Path(usize),
    At {
        dir: usize,
        path: usize,
    },
    /// As `At`, naming the descriptor itself where the path is empty and the
    /// flags at `flags` hold `AT_EMPTY_PATH`.
    AtOrDir {
        dir: usize,
        path: usize,
        flags: usize,
    },
}

/// Where a call takes its flags: an argument, or nowhere, for a call whose
/// flags are fixed.
#[derive(Clone, Copy)]
enum Flags {
    Arg(usize),
    Fixed(libc::c_int),
}

/// What a file call does with a path, as the table states it.
#[derive(Clone, Copy)]
enum Asks {
    /// Execute what the path leads to, or the path itself under
    /// `AT_SYMLINK_NOFOLLOW`.
    Exec(Flags),
    /// Remove the path itself, a directory under `AT_REMOVEDIR`.
    Remove(Flags),
    /// Move the path itself.
    MoveFrom,
    /// Link to the path itself, or to what it leads to under
    /// `AT_SYMLINK_FOLLOW`.
    LinkFrom(Flags),
    /// Make the path a directory.
    MakeDir,
    /// Make the path a link.
    MakeLink,
    /// Make the path a node of the type the mode at this argument names.
    MakeNode(usize),
    /// Move onto the path, as the `renameat2` flags say.
    RenameTo(Flags),
    /// Truncate what the path leads to.
    Truncate,
    /// As the `open` flags say.
    Open(Flags),
    /// As the flags of the `open_how` this argument points to say.
    OpenHow(usize),
}

/// What a file call does with a path, its flags read.
#[derive(Clone, Copy)]
enum Ask {
    Exec {
        follow: bool,
    },
    Link {
        follow: bool,
    },
    Unlink,
    RemoveDir,
    /// Move the entry away: a rename's source, or either side of an exchange.
    Move,
    /// `slash`: whether a trailing `/` may name the entry, as it may a
    /// directory made or moved there. Any other fails with `ENOENT`.
    Create {
        slash: bool,
    },
    Replace,
    Truncate,
    Open(libc::c_int),
}

/// `O_TMPFILE` without the `O_DIRECTORY` it carries: the bit that asks for one.
const TMPFILE: libc::c_int = libc::O_TMPFILE & !libc::O_DIRECTORY;

/// How a call spelled the end of its path, which resolution forgets.
#[derive(Clone, Copy)]
struct Named {
    /// The last component is `.` or `..`.
    dot: bool,
    /// The path ends in `/`, asking for a directory.
    slash: bool,
}

impl Named {
    fn of(path: &Path) -> Self {
        let bytes = path.as_os_str().as_bytes();
        let end = bytes
            .iter()
            .rposition(|&byte| byte != b'/')
            .map_or(0, |last| last + 1);
        let last = bytes[..end]
            .rsplit(|&byte| byte == b'/')
            .next()
            .unwrap_or_default();
        Self {
            dot: matches!(last, b"." | b".."),
            // The root alone is no directory asked for.
            slash: end > 0 && end < bytes.len(),
        }
    }
}

/// What a path names once resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Found {
    Absent,
    /// A symbolic link the call does not follow.
    Link,
    Dir,
    File,
    /// A FIFO, a device or a socket, which `exec` and `truncate` refuse before
    /// Landlock's hook.
    Special,
}

impl Ask {
    /// Whether the call follows a link its last component names. A lookup,
    /// unlike a call that makes, removes or moves an entry, follows one that a
    /// trailing `/` names whatever its flags.
    fn follows(self, named: Named) -> bool {
        match self {
            Self::Truncate => true,
            Self::Exec { follow } | Self::Link { follow } => follow || named.slash,
            Self::Unlink | Self::RemoveDir | Self::Move | Self::Create { .. } | Self::Replace => {
                false
            }
            Self::Open(flags) if flags & libc::O_CREAT != 0 => {
                flags & libc::O_NOFOLLOW == 0 && !Self::exclusive(flags)
            }
            Self::Open(flags) => flags & libc::O_NOFOLLOW == 0 || named.slash,
        }
    }

    fn exclusive(flags: libc::c_int) -> bool {
        flags & (libc::O_CREAT | libc::O_EXCL) == libc::O_CREAT | libc::O_EXCL
    }

    /// The directory or file Landlock judges the call on and the access it
    /// asks there, or `None` where the call fails before Landlock's hook, as
    /// `fs/namei.c` orders it. A call that makes, removes or moves an entry is
    /// judged on the directory holding it; one that opens a file, on the file.
    /// No call makes, removes or moves the root, `.` or `..` (`EEXIST`,
    /// `EBUSY`, `EISDIR`, `EINVAL`, `ENOTEMPTY`). A trailing `/` fails
    /// `unlink` (`EISDIR`, `ENOTDIR`), `rmdir` ignores it, and any other call
    /// it leaves on what is no directory fails with `ENOTDIR`, as does
    /// `O_DIRECTORY`. `O_TMPFILE` makes a file in the directory it opens,
    /// which is judged as the file.
    fn judged(self, path: &Path, found: Found, named: Named) -> Option<(PathBuf, Access)> {
        let entry = !named.dot && path != Path::new("/");
        let wants_dir = match self {
            Self::RemoveDir | Self::Replace => false,
            Self::Open(flags) => named.slash || flags & libc::O_DIRECTORY != 0,
            Self::Exec { .. }
            | Self::Link { .. }
            | Self::Unlink
            | Self::Move
            | Self::Create { .. }
            | Self::Truncate => named.slash,
        };
        if wants_dir && !matches!(found, Found::Dir | Found::Absent) {
            return None;
        }
        let itself = |access| Some((path.to_owned(), access));
        let parent = || {
            let parent = path.parent().expect("an entry has a parent");
            Some((parent.to_owned(), Access::ReadWrite))
        };
        match self {
            Self::Exec { .. } if found == Found::File => itself(Access::ReadExecute),
            Self::Truncate if found == Found::File => itself(Access::ReadWrite),
            Self::Exec { .. } | Self::Truncate => None,
            Self::Create { slash }
                if entry && found == Found::Absent && (slash || !named.slash) =>
            {
                parent()
            }
            Self::Unlink if named.slash => None,
            Self::Link { .. } | Self::Unlink | Self::RemoveDir | Self::Move
                if entry && found != Found::Absent =>
            {
                parent()
            }
            Self::Replace if entry => parent(),
            Self::Create { .. }
            | Self::Link { .. }
            | Self::Unlink
            | Self::RemoveDir
            | Self::Move
            | Self::Replace => None,
            // Asking both fails with `EINVAL`, and a trailing `/` with `EISDIR`.
            Self::Open(flags)
                if flags & libc::O_CREAT != 0
                    && (flags & libc::O_DIRECTORY != 0 || named.slash) =>
            {
                None
            }
            Self::Open(flags) if flags & TMPFILE != 0 => {
                // Anything but `O_TMPFILE` for writing fails with `EINVAL`.
                let valid = flags & (libc::O_TMPFILE | libc::O_CREAT) == libc::O_TMPFILE
                    && flags & libc::O_ACCMODE != libc::O_RDONLY;
                if valid && found == Found::Dir {
                    itself(Access::ReadWrite)
                } else {
                    None
                }
            }
            Self::Open(flags) => match found {
                Found::Absent if flags & libc::O_CREAT != 0 => parent(),
                Found::Absent | Found::Link => None,
                Found::Dir if flags & libc::O_CREAT != 0 || Self::opens_to_write(flags) => None,
                Found::File | Found::Special if Self::exclusive(flags) => None,
                Found::Dir | Found::File | Found::Special => {
                    itself(if Self::opens_to_write(flags) {
                        Access::ReadWrite
                    } else {
                        Access::ReadExecute
                    })
                }
            },
        }
    }

    /// Whether an open of an existing file asks to write, as Landlock judges
    /// it: `O_TRUNC` takes the truncate right.
    fn opens_to_write(flags: libc::c_int) -> bool {
        flags & libc::O_ACCMODE != libc::O_RDONLY || flags & libc::O_TRUNC != 0
    }
}

/// The calls whose paths Landlock mediates, each with the paths it names.
/// `symlink` names its target as text, never resolved, so only its link
/// counts.
const FILE_CALLS: &[(libc::c_long, &[(PathArg, Asks)])] = &[
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_open,
        &[(PathArg::Path(0), Asks::Open(Flags::Arg(1)))],
    ),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_creat,
        &[(
            PathArg::Path(0),
            Asks::Open(Flags::Fixed(libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC)),
        )],
    ),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_mkdir, &[(PathArg::Path(0), Asks::MakeDir)]),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_mknod, &[(PathArg::Path(0), Asks::MakeNode(1))]),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_unlink,
        &[(PathArg::Path(0), Asks::Remove(Flags::Fixed(0)))],
    ),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_rmdir,
        &[(
            PathArg::Path(0),
            Asks::Remove(Flags::Fixed(libc::AT_REMOVEDIR)),
        )],
    ),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_rename,
        &[
            (PathArg::Path(0), Asks::MoveFrom),
            (PathArg::Path(1), Asks::RenameTo(Flags::Fixed(0))),
        ],
    ),
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    (
        libc::SYS_renameat,
        &[
            (PathArg::At { dir: 0, path: 1 }, Asks::MoveFrom),
            (
                PathArg::At { dir: 2, path: 3 },
                Asks::RenameTo(Flags::Fixed(0)),
            ),
        ],
    ),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_link,
        &[
            (PathArg::Path(0), Asks::LinkFrom(Flags::Fixed(0))),
            (PathArg::Path(1), Asks::MakeLink),
        ],
    ),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_symlink, &[(PathArg::Path(1), Asks::MakeLink)]),
    (
        libc::SYS_openat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::Open(Flags::Arg(2)))],
    ),
    (
        libc::SYS_openat2,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::OpenHow(2))],
    ),
    (
        libc::SYS_execve,
        &[(PathArg::Path(0), Asks::Exec(Flags::Fixed(0)))],
    ),
    (
        libc::SYS_execveat,
        &[(
            PathArg::AtOrDir {
                dir: 0,
                path: 1,
                flags: 4,
            },
            Asks::Exec(Flags::Arg(4)),
        )],
    ),
    (
        libc::SYS_mkdirat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::MakeDir)],
    ),
    (
        libc::SYS_mknodat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::MakeNode(2))],
    ),
    (
        libc::SYS_unlinkat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::Remove(Flags::Arg(2)))],
    ),
    (
        libc::SYS_renameat2,
        &[
            (PathArg::At { dir: 0, path: 1 }, Asks::MoveFrom),
            (
                PathArg::At { dir: 2, path: 3 },
                Asks::RenameTo(Flags::Arg(4)),
            ),
        ],
    ),
    (
        libc::SYS_linkat,
        &[
            (
                PathArg::AtOrDir {
                    dir: 0,
                    path: 1,
                    flags: 4,
                },
                Asks::LinkFrom(Flags::Arg(4)),
            ),
            (PathArg::At { dir: 2, path: 3 }, Asks::MakeLink),
        ],
    ),
    (
        libc::SYS_symlinkat,
        &[(PathArg::At { dir: 1, path: 2 }, Asks::MakeLink)],
    ),
    (libc::SYS_truncate, &[(PathArg::Path(0), Asks::Truncate)]),
];

fn file_call(nr: libc::c_long) -> Option<&'static [(PathArg, Asks)]> {
    FILE_CALLS
        .iter()
        .find(|(call, _)| *call == nr)
        .map(|(_, paths)| *paths)
}

/// The paths of a file call the policy does not grant the access it asks.
/// A path the call fails on before Landlock judges it is left out, as [`Ask`]
/// says. So is one the caller's memory no longer holds, which a child that
/// exits mid-call leaves.
fn refused_paths(
    listener: &OwnedFd,
    notif: &libc::seccomp_notif,
    paths: &[(PathArg, Asks)],
    policy: &ResolvedPolicy,
) -> Result<Vec<Refusal>, Unseen> {
    let caller = Caller {
        pid: notif.pid as libc::pid_t,
        args: notif.data.args,
    };
    let asked = paths
        .iter()
        .map(|&(arg, asks)| Ok(caller.path(arg)?.zip(caller.ask(asks)?)))
        .collect::<Result<Option<Vec<(PathBuf, Ask)>>, Unseen>>();
    // The caller may have died and its pid passed on while we read its memory.
    // SAFETY: `notif.id` outlives the call.
    if unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            libc::SECCOMP_IOCTL_NOTIF_ID_VALID,
            &raw const notif.id,
        )
    } == -1
    {
        return Ok(Vec::new());
    }
    // A path the call fails on, or an `O_PATH` open, leaves nothing to judge.
    let Some(asked) = asked? else {
        return Ok(Vec::new());
    };
    // Landlock judges a call once, on all its paths, so a call that fails on
    // any of them first is judged on none.
    let Some(resolved) = asked
        .into_iter()
        .map(|(path, ask)| {
            let named = Named::of(&path);
            let (path, found) = caller.resolve(&path, ask.follows(named))?;
            Some((path, found, named, ask))
        })
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(Vec::new());
    };
    if let [(_, from, _, _), (_, _, to, to_ask)] = resolved.as_slice()
        && *from != Found::Dir
        && to.slash
        && !matches!(to_ask, Ask::Move)
    {
        // Moving anything but a directory onto a path ending in `/` fails
        // with `ENOTDIR`, and linking it there with `ENOENT`; an exchange
        // asks that of each side alone.
        return Ok(Vec::new());
    }
    let Some(judged) = resolved
        .into_iter()
        .map(|(path, found, named, ask)| {
            let (at, access) = ask.judged(&path, found, named)?;
            Some((path, found, ask, at, access))
        })
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(Vec::new());
    };
    if let [(_, _, _, from, _), (_, _, _, to, _)] = judged.as_slice()
        && !same_filesystem(from, to)
    {
        // A rename or link between filesystems fails with `EXDEV`.
        return Ok(Vec::new());
    }
    let mut refused = Vec::new();
    for (path, found, ask, at, access) in judged {
        if !policy.permits(&at, access) {
            // Judged on a directory: the one holding an entry, or one opened.
            let dir = (at != path || found == Found::Dir).then_some(at);
            refused.push(path_refusal(path, dir, access));
        } else if let Ask::Exec { .. } = ask
            && let Some(interpreter) = policy.refused_interpreter(&path)
        {
            refused.push(path_refusal(interpreter, None, Access::ReadExecute));
        }
    }
    Ok(refused)
}

/// `path` refused `access` where Landlock judged it: on the directory `dir`,
/// or else on `path` itself, a file whose directory a grant must cover.
fn path_refusal(path: PathBuf, dir: Option<PathBuf>, access: Access) -> Refusal {
    let dir = dir.unwrap_or_else(|| path.parent().expect("a file has a parent").to_owned());
    if is_process_entry(&path) {
        Refusal::ProcessEntry { access }
    } else {
        Refusal::Path { path, dir, access }
    }
}

fn same_filesystem(a: &Path, b: &Path) -> bool {
    let device = |path: &Path| std::fs::metadata(path).map(|meta| meta.dev()).ok();
    device(a) == device(b)
}

/// A task stopped in a file call the filter referred to the host.
struct Caller {
    pid: libc::pid_t,
    args: [u64; 6],
}

impl Caller {
    /// The path `arg` names, made absolute against the caller's working
    /// directory or the descriptor it names. `None` where the call fails on
    /// it, and for a descriptor that names no path (a pipe, a socket), which
    /// Landlock does not mediate.
    fn path(&self, arg: PathArg) -> Result<Option<PathBuf>, Unseen> {
        let (dir, path, empty) = match arg {
            PathArg::Path(path) => (libc::AT_FDCWD, path, false),
            PathArg::At { dir, path } => (self.args[dir] as libc::c_int, path, false),
            PathArg::AtOrDir { dir, path, flags } => (
                self.args[dir] as libc::c_int,
                path,
                self.args[flags] as libc::c_int & libc::AT_EMPTY_PATH != 0,
            ),
        };
        let Some(path) = self.read_c_string(self.args[path])? else {
            return Ok(None);
        };
        let path = PathBuf::from(OsString::from_vec(path));
        if path.is_absolute() {
            return Ok(Some(path));
        }
        // An empty path fails with `ENOENT` unless it names the descriptor.
        if path.as_os_str().is_empty() && !empty {
            return Ok(None);
        }
        let base = match dir {
            libc::AT_FDCWD => format!("/proc/{}/cwd", self.pid),
            fd => format!("/proc/{}/fd/{fd}", self.pid),
        };
        let Some(base) = seen(magic_target(Path::new(&base)))?.flatten() else {
            return Ok(None);
        };
        Ok(Some(if path.as_os_str().is_empty() {
            base
        } else {
            base.join(path)
        }))
    }

    /// `path` resolved one component at a time, as the kernel resolves it for
    /// the caller rather than for the host: `/proc/self` and the links through
    /// it (`/dev/stdout`, `/proc/mounts`) name the caller. The final component
    /// is followed only where `follow` says. `None` where resolution fails
    /// before Landlock judges the call, and for a descriptor that names no
    /// path (a pipe, a socket), which Landlock does not mediate.
    fn resolve(&self, path: &Path, follow: bool) -> Option<(PathBuf, Found)> {
        /// Linux's `MAXSYMLINKS`, past which a resolution fails with `ELOOP`.
        const MAX_LINKS: usize = 40;
        // Every component as spelled, `.` included: a component before it is
        // looked up as a directory, which `Path::components` would forget.
        let names = |path: &Path| -> Vec<OsString> {
            path.as_os_str()
                .as_bytes()
                .split(|&byte| byte == b'/')
                .filter(|name| !name.is_empty())
                .rev()
                .map(|name| OsString::from_vec(name.to_vec()))
                .collect()
        };
        let mut pending = names(path);
        let mut real = PathBuf::from("/");
        let mut links = 0;
        while let Some(name) = pending.pop() {
            if name == "." {
                continue;
            }
            if name == ".." {
                real.pop();
                continue;
            }
            let last = pending.is_empty();
            let next = self.entry(&real, &name)?;
            let meta = match std::fs::symlink_metadata(&next) {
                Ok(meta) => meta,
                Err(err) if last && err.kind() == io::ErrorKind::NotFound => {
                    return Some((next, Found::Absent));
                }
                Err(_) => return None,
            };
            if !meta.is_symlink() {
                if last {
                    let found = if meta.is_dir() {
                        Found::Dir
                    } else if meta.is_file() {
                        Found::File
                    } else {
                        Found::Special
                    };
                    return Some((next, found));
                }
                // A component that leads on must be a directory (`ENOTDIR`).
                if !meta.is_dir() {
                    return None;
                }
                real = next;
                continue;
            }
            if last && !follow {
                return Some((next, Found::Link));
            }
            links += 1;
            if links > MAX_LINKS {
                return None;
            }
            let target = if is_process_entry(&next) {
                magic_target(&next).ok().flatten()?
            } else {
                std::fs::read_link(&next).ok()?
            };
            if target.is_absolute() {
                real = PathBuf::from("/");
            }
            pending.extend(names(&target));
        }
        Some((real, Found::Dir))
    }

    /// `name` beneath the resolved directory `dir`, with `/proc/self` and
    /// `/proc/thread-self` naming the caller.
    fn entry(&self, dir: &Path, name: &OsString) -> Option<PathBuf> {
        if dir != Path::new("/proc") {
            return Some(dir.join(name));
        }
        Some(match name.to_str() {
            Some("self") => PathBuf::from(format!("/proc/{}", self.tgid()?)),
            Some("thread-self") => {
                PathBuf::from(format!("/proc/{}/task/{}", self.tgid()?, self.pid))
            }
            _ => dir.join(name),
        })
    }

    /// The process the calling thread belongs to: the notification names the
    /// thread.
    fn tgid(&self) -> Option<libc::pid_t> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.pid)).ok()?;
        status
            .lines()
            .find_map(|line| line.strip_prefix("Tgid:"))?
            .trim()
            .parse()
            .ok()
    }

    /// What `asks` does, its flags read, or `None` where the call fails on
    /// them first, and for an `O_PATH` open, which Landlock lets through.
    fn ask(&self, asks: Asks) -> Result<Option<Ask>, Unseen> {
        let flags = |flags: Flags| match flags {
            Flags::Arg(arg) => self.args[arg] as libc::c_int,
            Flags::Fixed(flags) => flags,
        };
        let open_flags = match asks {
            Asks::Exec(exec) => {
                return Ok(Some(Ask::Exec {
                    follow: flags(exec) & libc::AT_SYMLINK_NOFOLLOW == 0,
                }));
            }
            Asks::Remove(remove) => {
                return Ok(Some(if flags(remove) & libc::AT_REMOVEDIR != 0 {
                    Ask::RemoveDir
                } else {
                    Ask::Unlink
                }));
            }
            Asks::MoveFrom => return Ok(Some(Ask::Move)),
            Asks::LinkFrom(link) => {
                return Ok(Some(Ask::Link {
                    follow: flags(link) & libc::AT_SYMLINK_FOLLOW != 0,
                }));
            }
            Asks::MakeDir => return Ok(Some(Ask::Create { slash: true })),
            Asks::MakeLink => return Ok(Some(Ask::Create { slash: false })),
            // A directory, or a type that names none, fails with `EPERM` or
            // `EINVAL`.
            Asks::MakeNode(arg) => {
                let kind = self.args[arg] as libc::mode_t & libc::S_IFMT;
                let made = matches!(
                    kind,
                    0 | libc::S_IFREG
                        | libc::S_IFCHR
                        | libc::S_IFBLK
                        | libc::S_IFIFO
                        | libc::S_IFSOCK
                );
                return Ok(made.then_some(Ask::Create { slash: false }));
            }
            Asks::RenameTo(rename) => {
                let rename = flags(rename) as libc::c_uint;
                return Ok(Some(if rename & libc::RENAME_NOREPLACE != 0 {
                    Ask::Create { slash: true }
                } else if rename & libc::RENAME_EXCHANGE != 0 {
                    Ask::Move
                } else {
                    Ask::Replace
                }));
            }
            Asks::Truncate => return Ok(Some(Ask::Truncate)),
            Asks::Open(open) => flags(open),
            Asks::OpenHow(arg) => {
                let mut how = [0u8; size_of::<u64>()];
                if seen(self.read(self.args[arg], &mut how))?.is_none() {
                    return Ok(None);
                }
                u64::from_ne_bytes(how) as libc::c_int
            }
        };
        Ok((open_flags & libc::O_PATH == 0).then_some(Ask::Open(open_flags)))
    }

    /// The NUL-terminated string at `addr` in the caller's memory, up to
    /// `PATH_MAX`, the longest path a call accepts.
    fn read_c_string(&self, addr: u64) -> Result<Option<Vec<u8>>, Unseen> {
        let mut bytes = vec![0u8; libc::PATH_MAX as usize];
        let Some(read) = seen(self.read(addr, &mut bytes))? else {
            return Ok(None);
        };
        Ok(bytes[..read].iter().position(|&byte| byte == 0).map(|end| {
            bytes.truncate(end);
            bytes
        }))
    }

    /// Read from `addr` in the caller's memory into `buf`, returning how many
    /// bytes were read: a read that reaches an unmapped page stops there.
    fn read(&self, addr: u64, buf: &mut [u8]) -> io::Result<usize> {
        let local = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let remote = libc::iovec {
            iov_base: addr as *mut libc::c_void,
            iov_len: buf.len(),
        };
        // SAFETY: `local` covers `buf`, which outlives the call; the kernel
        // checks `remote` against the caller's own mappings.
        let read = unsafe {
            libc::process_vm_readv(self.pid, &raw const local, 1, &raw const remote, 1, 0)
        };
        usize::try_from(read).map_err(|_| io::Error::last_os_error())
    }
}

/// A caller the host may not inspect: Yama's `ptrace_scope`, or a process
/// that made itself non-dumpable.
struct Unseen;

/// What a look into the caller found, `None` where the caller's state ended
/// it (a fault, a closed descriptor, an exit).
fn seen<T>(result: io::Result<T>) -> Result<Option<T>, Unseen> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if matches!(err.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => Err(Unseen),
        Err(_) => Ok(None),
    }
}

/// The path a link in a process's `/proc` entry (`cwd`, `fd/N`) spells, where
/// it still leads to the object the link holds. The kernel follows such a link
/// to its object, never its text, which for a pipe, a memfd or a deleted file
/// names no path at all, or another file.
fn magic_target(link: &Path) -> io::Result<Option<PathBuf>> {
    let target = std::fs::read_link(link)?;
    let object = std::fs::metadata(link)?;
    let spelled = std::fs::metadata(&target);
    Ok((target.is_absolute()
        && spelled.is_ok_and(|named| named.dev() == object.dev() && named.ino() == object.ino()))
    .then_some(target))
}

/// Whether `path` lies in a process's `/proc` entry.
fn is_process_entry(path: &Path) -> bool {
    path.strip_prefix("/proc")
        .ok()
        .and_then(|rest| rest.components().next())
        .is_some_and(|pid| {
            pid.as_os_str()
                .to_str()
                .is_some_and(|pid| pid.parse::<libc::pid_t>().is_ok())
        })
}

pub(super) fn descriptor_path(file: &File) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn std_stdio(stdio: Stdio) -> StdStdio {
    match stdio {
        Stdio::Null => StdStdio::null(),
        Stdio::Inherit => StdStdio::inherit(),
        Stdio::Piped => StdStdio::piped(),
    }
}

/// Runs in the child between `fork` and `exec`.
fn confine_self(
    ruleset: &OwnedFd,
    filter: &[libc::sock_filter],
    cwd: Option<&File>,
    listener_tx: Option<RawFd>,
    terminal: Terminal,
) -> io::Result<()> {
    // A group of its own, so the host's kill reaches every descendant; the
    // filter below keeps them from leaving it.
    // SAFETY: each call below is a plain system call on memory the parent built.
    check(unsafe { libc::setpgid(0, 0) })?;
    if let Terminal::HandedOver(fd) = terminal {
        // Taken before `exec`, so the program's first read finds its group in
        // the foreground. Where the terminal refuses, the child runs in the
        // background, as under a host that does not hold it.
        let _ = give_terminal(fd, unsafe { libc::getpid() });
    }
    if let Some(dir) = cwd {
        check(unsafe { libc::fchdir(dir.as_raw_fd()) })?;
    }
    check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;
    grant_own_proc(ruleset)?;
    check_long(unsafe {
        libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32)
    })?;
    let flags = match listener_tx {
        Some(_) => libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
        None => 0,
    };
    let listener = install_filter(filter, flags)?;
    if let Some(listener_tx) = listener_tx {
        send_listener(listener_tx, listener)?;
        check(unsafe { libc::close(listener) })?;
    }
    // Confinement mediates what the child opens, never what it already holds.
    // Marked close-on-exec rather than closed, so the descriptor the standard
    // library reports a failed `exec` through survives until the `exec`.
    check_long(unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    })?;
    Ok(())
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn check_long(ret: libc::c_long) -> io::Result<()> {
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Confine the calling thread with `filter`, returning the listener
/// `SECCOMP_FILTER_FLAG_NEW_LISTENER` asks for. Safe between `fork` and `exec`.
fn install_filter(filter: &[libc::sock_filter], flags: libc::c_ulong) -> io::Result<RawFd> {
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).expect("the filter is built short"),
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: `program` points into `filter`, which outlives the call.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            flags,
            &raw const program,
        )
    };
    check_long(ret)?;
    Ok(ret as RawFd)
}

fn ruleset(policy: &Policy) -> io::Result<OwnedFd> {
    let attr = RulesetAttr {
        handled_access_fs: ACCESS_FS_ALL,
        handled_access_net: 0,
        scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
    };
    // SAFETY: `attr` outlives the call, which reads its size in bytes.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just returned this descriptor, close-on-exec, to us alone.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    for rule in policy.rules() {
        let opened: File;
        let file: &File = match &rule.dir {
            Some(dir) => dir,
            None => match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH)
                .open(&rule.path)
            {
                Ok(file) => {
                    opened = file;
                    &opened
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(io::Error::new(
                        err.kind(),
                        format!("{}: {err}", rule.path.display()),
                    ));
                }
            },
        };
        let mut allowed = match rule.access {
            Access::ReadExecute => ACCESS_FS_READ,
            Access::ReadWrite => ACCESS_FS_ALL,
        };
        if !file.metadata()?.is_dir() {
            allowed &= ACCESS_FS_FILE;
        }
        add_rule(&ruleset, file.as_raw_fd(), allowed)
            .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", rule.path.display())))?;
    }
    Ok(ruleset)
}

/// Grant `allowed` beneath the file `parent` names. The child calls it too,
/// between `fork` and `exec`.
fn add_rule(ruleset: &OwnedFd, parent: RawFd, allowed: u64) -> io::Result<()> {
    let beneath = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: parent,
    };
    // SAFETY: `beneath` outlives the call.
    check_long(unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &raw const beneath,
            0u32,
        )
    })
}

/// Let the child read its own `/proc` entry. Only the child can open it: in
/// the host, `/proc/self` names the host. A host without `/proc` grants
/// nothing.
fn grant_own_proc(ruleset: &OwnedFd) -> io::Result<()> {
    // SAFETY: a plain system call on a static string.
    let own = unsafe {
        libc::open(
            c"/proc/self".as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if own == -1 {
        let err = io::Error::last_os_error();
        return if err.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(err)
        };
    }
    let added = add_rule(ruleset, own, ACCESS_FS_READ);
    // SAFETY: `own` is ours, opened above.
    check(unsafe { libc::close(own) })?;
    added
}

/// System calls refused whatever the policy grants.
const ALWAYS_DENIED: &[libc::c_long] = &[
    libc::SYS_setsid,
    libc::SYS_setpgid,
    libc::SYS_ptrace,
    libc::SYS_io_uring_setup,
];

const SECCOMP_DATA_NR: u32 = 0;
const SECCOMP_DATA_ARCH: u32 = 4;
/// The low half of a system call argument, on a little-endian machine.
const fn seccomp_data_arg(index: u32) -> u32 {
    16 + 8 * index
}

/// The filter, answering what it refuses with `refer`: a referral to the
/// host's listener, or `EPERM` where the child takes none.
fn seccomp_filter(network: Network, trace: Trace, refer: u32) -> Vec<libc::sock_filter> {
    let arch = AUDIT_ARCH.expect("availability refuses an architecture without a filter");
    let mut asm = Assembler::default();
    asm.load(SECCOMP_DATA_ARCH);
    asm.jump_if_eq(arch, Label::ArchOk);
    asm.ret(libc::SECCOMP_RET_KILL_PROCESS);
    asm.label(Label::ArchOk);
    asm.load(SECCOMP_DATA_NR);
    if let Some(bit) = X32_SYSCALL_BIT {
        asm.jump_if_ge(bit, Label::Kill);
    }
    for &nr in ALWAYS_DENIED {
        asm.jump_if_eq(nr as u32, Label::Refer);
    }
    match trace {
        Trace::Off => {}
        Trace::Files => {
            for &(nr, _) in FILE_CALLS {
                asm.jump_if_eq(nr as u32, Label::Refer);
            }
        }
    }
    asm.jump_if_eq(libc::SYS_socket as u32, Label::Socket);
    asm.jump_if_eq(libc::SYS_ioctl as u32, Label::Ioctl);
    asm.ret(libc::SECCOMP_RET_ALLOW);

    asm.label(Label::Socket);
    match network {
        Network::Denied => asm.ret(refer),
        Network::Granted => {
            // A UNIX socket named by a path reaches a service outside the
            // sandbox; `socketpair` stays, reaching nothing outside the child.
            asm.load(seccomp_data_arg(0));
            asm.jump_if_eq(libc::AF_UNIX as u32, Label::Refer);
            asm.ret(libc::SECCOMP_RET_ALLOW);
        }
    }

    asm.label(Label::Ioctl);
    asm.load(seccomp_data_arg(1));
    asm.jump_if_eq(libc::TIOCSTI as u32, Label::Refer);
    asm.jump_if_eq(libc::TIOCLINUX as u32, Label::Refer);
    asm.ret(libc::SECCOMP_RET_ALLOW);

    // The host refuses the call, or under `Trace::Files` lets a file call
    // through for Landlock to judge.
    asm.label(Label::Refer);
    asm.ret(refer);
    asm.label(Label::Kill);
    asm.ret(libc::SECCOMP_RET_KILL_PROCESS);
    asm.finish()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Label {
    ArchOk,
    Socket,
    Ioctl,
    Refer,
    Kill,
}

enum Insn {
    Load(u32),
    JumpIfEq(u32, Label),
    JumpIfGe(u32, Label),
    Ret(u32),
    Label(Label),
}

/// Classic BPF with forward jumps to named labels, resolved in `finish`.
#[derive(Default)]
struct Assembler {
    insns: Vec<Insn>,
}

impl Assembler {
    fn load(&mut self, offset: u32) {
        self.insns.push(Insn::Load(offset));
    }

    fn jump_if_eq(&mut self, k: u32, target: Label) {
        self.insns.push(Insn::JumpIfEq(k, target));
    }

    fn jump_if_ge(&mut self, k: u32, target: Label) {
        self.insns.push(Insn::JumpIfGe(k, target));
    }

    fn ret(&mut self, k: u32) {
        self.insns.push(Insn::Ret(k));
    }

    fn label(&mut self, label: Label) {
        self.insns.push(Insn::Label(label));
    }

    fn finish(self) -> Vec<libc::sock_filter> {
        let mut index: usize = 0;
        let mut positions: Vec<(Label, usize)> = Vec::new();
        for insn in &self.insns {
            match insn {
                Insn::Label(label) => positions.push((*label, index)),
                Insn::Load(_) | Insn::JumpIfEq(..) | Insn::JumpIfGe(..) | Insn::Ret(_) => {
                    index += 1;
                }
            }
        }
        let position = |target: Label| {
            positions
                .iter()
                .find(|(label, _)| *label == target)
                .map(|(_, at)| *at)
                .expect("every jump names a placed label")
        };
        let mut out: Vec<libc::sock_filter> = Vec::new();
        for insn in &self.insns {
            let at = out.len();
            let jump = |code: u32, k: u32, target: Label| {
                let offset = position(target)
                    .checked_sub(at + 1)
                    .expect("every jump is forward");
                libc::sock_filter {
                    code: code as u16,
                    jt: u8::try_from(offset).expect("the filter is built short"),
                    jf: 0,
                    k,
                }
            };
            match *insn {
                Insn::Load(offset) => out.push(libc::sock_filter {
                    code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                    jt: 0,
                    jf: 0,
                    k: offset,
                }),
                Insn::JumpIfEq(k, target) => {
                    out.push(jump(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, k, target));
                }
                Insn::JumpIfGe(k, target) => {
                    out.push(jump(libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K, k, target));
                }
                Insn::Ret(k) => out.push(libc::sock_filter {
                    code: (libc::BPF_RET | libc::BPF_K) as u16,
                    jt: 0,
                    jf: 0,
                    k,
                }),
                Insn::Label(_) => {}
            }
        }
        out
    }
}
