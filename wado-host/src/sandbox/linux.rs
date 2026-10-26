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
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio as StdStdio};
use std::ptr;
use std::sync::OnceLock;
use std::thread;

use super::{
    Access, Availability, Child, Command, Network, Policy, Refusal, Refusals, ResolvedPolicy,
    Spawned, Stdio, Trace,
};

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

pub(super) fn spawn(command: &Command<'_>) -> io::Result<Spawned> {
    let ruleset = ruleset(command.policy)?;
    let filter = seccomp_filter(command.policy.network(), command.trace);
    let traced = match command.trace {
        Trace::Off => None,
        Trace::Files => Some(command.policy.resolved()),
    };
    let [listener_tx, listener_rx] = socket_pair()?;
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
            confine_self(&ruleset, &filter, cwd.as_ref(), listener_tx.as_raw_fd())
        });
    }
    let refusals = Refusals::default();
    let recorded = refusals.clone();
    // Started first: under `Trace::Files` the child's own `exec` waits on it.
    thread::Builder::new()
        .name("sandbox-refusals".to_owned())
        .spawn(move || {
            // `None` once every sender is closed: the spawn failed before the
            // child confined itself.
            if let Some((listener, pid)) = receive_listener(&listener_rx) {
                let traced = traced.map(|mut policy| {
                    policy.grant_own_proc(pid);
                    policy
                });
                answer_refusals(&listener, &recorded, traced.as_ref());
            }
        })?;
    let spawned = std_command.spawn();
    // Closes the host's copy of the sender, which the closure holds.
    drop(std_command);
    let mut child = spawned?;
    Ok(Spawned {
        child: Child::new(child.id() as libc::pid_t),
        stdin: child.stdin.take().map(OwnedFd::from),
        stdout: child.stdout.take().map(OwnedFd::from),
        stderr: child.stderr.take().map(OwnedFd::from),
        refusals,
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

/// Runs in the child between `fork` and `exec`.
fn send_listener(socket: RawFd, listener: RawFd) -> io::Result<()> {
    let mut message = FdMessage::new();
    // SAFETY: a plain system call.
    message.pid = unsafe { libc::getpid() };
    let header = message.header();
    // SAFETY: the control buffer holds one header and one descriptor, and
    // `message` outlives the calls.
    let sent = unsafe {
        let control = libc::CMSG_FIRSTHDR(header);
        (*control).cmsg_level = libc::SOL_SOCKET;
        (*control).cmsg_type = libc::SCM_RIGHTS;
        (*control).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        libc::CMSG_DATA(control)
            .cast::<RawFd>()
            .write_unaligned(listener);
        libc::sendmsg(socket, header, 0)
    };
    if sent == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The listener the child sends, and its pid, or `None` once no sender is left.
fn receive_listener(socket: &OwnedFd) -> Option<(OwnedFd, libc::pid_t)> {
    let mut message = FdMessage::new();
    let header = message.header();
    let received = loop {
        // SAFETY: `message` outlives the call.
        let received = unsafe { libc::recvmsg(socket.as_raw_fd(), header, libc::MSG_CMSG_CLOEXEC) };
        if received != -1 {
            break received;
        }
        let err = io::Error::last_os_error();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted, "recvmsg: {err}");
    };
    if received == 0 {
        return None;
    }
    // SAFETY: the kernel filled the control buffer `header` points to.
    let control = unsafe { libc::CMSG_FIRSTHDR(header) };
    assert!(!control.is_null(), "the child sends its listener");
    // SAFETY: a header the kernel wrote, holding the one descriptor sent, which
    // `SCM_RIGHTS` installed in our table for us alone.
    let listener =
        unsafe { OwnedFd::from_raw_fd(libc::CMSG_DATA(control).cast::<RawFd>().read_unaligned()) };
    Some((listener, message.pid))
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
        if unsafe { libc::poll(&raw mut ready, 1, -1) } == -1 {
            let err = io::Error::last_os_error();
            assert_eq!(err.kind(), io::ErrorKind::Interrupted, "poll: {err}");
            continue;
        }
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
            for refusal in refused_paths(listener, &notif, paths, policy) {
                refusals.record(refusal);
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
    At { dir: usize, path: usize },
}

/// The access a file call asks of its path.
#[derive(Clone, Copy)]
enum Asks {
    Read,
    Write,
    /// As the `open` flags in this argument say.
    OpenFlags(usize),
    /// As the flags of the `open_how` this argument points to say.
    OpenHow(usize),
}

/// The calls whose paths Landlock mediates, each with the paths it names.
/// `symlink` names its target as text, never resolved, so only its link
/// counts.
const FILE_CALLS: &[(libc::c_long, &[(PathArg, Asks)])] = &[
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_open, &[(PathArg::Path(0), Asks::OpenFlags(1))]),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_creat, &[(PathArg::Path(0), Asks::Write)]),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_mkdir, &[(PathArg::Path(0), Asks::Write)]),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_mknod, &[(PathArg::Path(0), Asks::Write)]),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_unlink, &[(PathArg::Path(0), Asks::Write)]),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_rmdir, &[(PathArg::Path(0), Asks::Write)]),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_rename,
        &[
            (PathArg::Path(0), Asks::Write),
            (PathArg::Path(1), Asks::Write),
        ],
    ),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_renameat,
        &[
            (PathArg::At { dir: 0, path: 1 }, Asks::Write),
            (PathArg::At { dir: 2, path: 3 }, Asks::Write),
        ],
    ),
    #[cfg(target_arch = "x86_64")]
    (
        libc::SYS_link,
        &[
            (PathArg::Path(0), Asks::Write),
            (PathArg::Path(1), Asks::Write),
        ],
    ),
    #[cfg(target_arch = "x86_64")]
    (libc::SYS_symlink, &[(PathArg::Path(1), Asks::Write)]),
    (
        libc::SYS_openat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::OpenFlags(2))],
    ),
    (
        libc::SYS_openat2,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::OpenHow(2))],
    ),
    (libc::SYS_execve, &[(PathArg::Path(0), Asks::Read)]),
    (
        libc::SYS_execveat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::Read)],
    ),
    (
        libc::SYS_mkdirat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::Write)],
    ),
    (
        libc::SYS_mknodat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::Write)],
    ),
    (
        libc::SYS_unlinkat,
        &[(PathArg::At { dir: 0, path: 1 }, Asks::Write)],
    ),
    (
        libc::SYS_renameat2,
        &[
            (PathArg::At { dir: 0, path: 1 }, Asks::Write),
            (PathArg::At { dir: 2, path: 3 }, Asks::Write),
        ],
    ),
    (
        libc::SYS_linkat,
        &[
            (PathArg::At { dir: 0, path: 1 }, Asks::Write),
            (PathArg::At { dir: 2, path: 3 }, Asks::Write),
        ],
    ),
    (
        libc::SYS_symlinkat,
        &[(PathArg::At { dir: 1, path: 2 }, Asks::Write)],
    ),
    (libc::SYS_truncate, &[(PathArg::Path(0), Asks::Write)]),
];

fn file_call(nr: libc::c_long) -> Option<&'static [(PathArg, Asks)]> {
    FILE_CALLS
        .iter()
        .find(|(call, _)| *call == nr)
        .map(|(_, paths)| *paths)
}

/// The paths of a file call the policy does not grant the access it asks.
/// A path that cannot exist for the call is left out: the call fails with
/// `ENOENT` before Landlock judges it. So is one the caller's memory no longer
/// holds, which a child that exits mid-call leaves.
fn refused_paths(
    listener: &OwnedFd,
    notif: &libc::seccomp_notif,
    paths: &[(PathArg, Asks)],
    policy: &ResolvedPolicy,
) -> Vec<Refusal> {
    let caller = Caller {
        pid: notif.pid as libc::pid_t,
        args: notif.data.args,
    };
    let asked: Vec<(PathBuf, Access)> = paths
        .iter()
        .filter_map(|&(arg, asks)| Some((caller.path(arg)?, caller.access(asks)?)))
        .collect();
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
        return Vec::new();
    }
    asked
        .into_iter()
        .filter_map(|(path, access)| {
            let path = real_path(&path, access)?;
            (!policy.permits(&path, access)).then_some(Refusal::Path { path, access })
        })
        .collect()
}

/// A task stopped in a file call the filter referred to the host.
struct Caller {
    pid: libc::pid_t,
    args: [u64; 6],
}

impl Caller {
    /// The path `arg` names, made absolute against the caller's working
    /// directory or the descriptor it names.
    fn path(&self, arg: PathArg) -> Option<PathBuf> {
        let (dir, path) = match arg {
            PathArg::Path(path) => (libc::AT_FDCWD, path),
            PathArg::At { dir, path } => (self.args[dir] as libc::c_int, path),
        };
        let path = PathBuf::from(OsString::from_vec(self.read_c_string(self.args[path])?));
        if path.is_absolute() {
            return self.own_proc(path);
        }
        let base = match dir {
            libc::AT_FDCWD => format!("/proc/{}/cwd", self.pid),
            fd => format!("/proc/{}/fd/{fd}", self.pid),
        };
        self.own_proc(std::fs::read_link(base).ok()?.join(path))
    }

    /// `path` with an entry of `/proc` that leads through `self` or
    /// `thread-self` (`/proc/mounts` is `self/mounts`) naming the caller, as it
    /// does for the caller, rather than the host that resolves the path.
    fn own_proc(&self, path: PathBuf) -> Option<PathBuf> {
        let Ok(under_proc) = path.strip_prefix("/proc") else {
            return Some(path);
        };
        let mut parts = under_proc.components();
        let Some(entry) = parts.next() else {
            return Some(path);
        };
        let entry = Path::new(entry.as_os_str());
        let target = if entry == "self" || entry == "thread-self" {
            entry.to_path_buf()
        } else {
            let Ok(target) = std::fs::read_link(Path::new("/proc").join(entry)) else {
                return Some(path);
            };
            target
        };
        let own = if let Ok(rest) = target.strip_prefix("thread-self") {
            Path::new(&format!("/proc/{}/task/{}", self.tgid()?, self.pid)).join(rest)
        } else if let Ok(rest) = target.strip_prefix("self") {
            Path::new(&format!("/proc/{}", self.tgid()?)).join(rest)
        } else {
            return Some(path);
        };
        Some(own.join(parts.as_path()))
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

    /// The access `asks` names, or `None` for an `O_PATH` open, which Landlock
    /// lets through.
    fn access(&self, asks: Asks) -> Option<Access> {
        let flags = match asks {
            Asks::Read => return Some(Access::ReadExecute),
            Asks::Write => return Some(Access::ReadWrite),
            Asks::OpenFlags(arg) => self.args[arg] as libc::c_int,
            Asks::OpenHow(arg) => {
                let mut how = [0u8; size_of::<u64>()];
                self.read(self.args[arg], &mut how)?;
                u64::from_ne_bytes(how) as libc::c_int
            }
        };
        if flags & libc::O_PATH != 0 {
            return None;
        }
        let writes = flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_CREAT | libc::O_TRUNC) != 0;
        Some(if writes {
            Access::ReadWrite
        } else {
            Access::ReadExecute
        })
    }

    /// The NUL-terminated string at `addr` in the caller's memory, up to
    /// `PATH_MAX`, the longest path a call accepts.
    fn read_c_string(&self, addr: u64) -> Option<Vec<u8>> {
        let mut bytes = vec![0u8; libc::PATH_MAX as usize];
        let read = self.read(addr, &mut bytes)?;
        let end = bytes[..read].iter().position(|&byte| byte == 0)?;
        bytes.truncate(end);
        Some(bytes)
    }

    /// Read from `addr` in the caller's memory into `buf`, returning how many
    /// bytes were read: a read that reaches an unmapped page stops there.
    fn read(&self, addr: u64, buf: &mut [u8]) -> Option<usize> {
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
        usize::try_from(read).ok()
    }
}

/// `path` at its real path, as Landlock judges it, or `None` where the call
/// fails before Landlock does: a read of a path that does not exist, or a
/// write beneath a directory that does not.
fn real_path(path: &Path, access: Access) -> Option<PathBuf> {
    if let Ok(real) = std::fs::canonicalize(path) {
        return Some(real);
    }
    if access == Access::ReadExecute {
        return None;
    }
    Some(
        std::fs::canonicalize(path.parent()?)
            .ok()?
            .join(path.file_name()?),
    )
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
    listener_tx: RawFd,
) -> io::Result<()> {
    // A group of its own, so the host's kill reaches every descendant; the
    // filter below keeps them from leaving it.
    // SAFETY: each call below is a plain system call on memory the parent built.
    check(unsafe { libc::setpgid(0, 0) })?;
    if let Some(dir) = cwd {
        check(unsafe { libc::fchdir(dir.as_raw_fd()) })?;
    }
    check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;
    grant_own_proc(ruleset)?;
    check_long(unsafe {
        libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32)
    })?;
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).expect("the filter is built short"),
        filter: filter.as_ptr().cast_mut(),
    };
    let listener = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &raw const program,
        )
    };
    check_long(listener)?;
    send_listener(listener_tx, listener as RawFd)?;
    check(unsafe { libc::close(listener as RawFd) })?;
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

fn seccomp_filter(network: Network, trace: Trace) -> Vec<libc::sock_filter> {
    let arch = AUDIT_ARCH.expect("availability refuses an architecture without a filter");
    let refer = libc::SECCOMP_RET_USER_NOTIF;
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
