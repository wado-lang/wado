//! The sandbox a spawned child runs in: confined to the guest's grants plus a
//! fixed system baseline, never more. See WEP 2026-10-21 (`core:process`).
//!
//! The parent stays unconfined. Each backend confines the child alone, without
//! re-executing `wado`, so a host embedded in another binary spawns as `wado`
//! does.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::sync::lock;

#[cfg(unix)]
mod exec;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use unsupported as backend;

/// Whether this host can confine a child, settled once per process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// The OS cannot confine a child; the reason names what it lacks.
    Unsupported(String),
}

#[must_use]
pub fn availability() -> Availability {
    backend::availability()
}

/// What a child may do beneath one subtree, the lesser first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    ReadExecute,
    /// Read, write, create, remove, and execute.
    ReadWrite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Denied,
    Granted,
}

/// One subtree a child may reach. `dir` is the descriptor the host already
/// holds for it, where there is one: Linux confines through it, so the path
/// is not resolved a second time.
#[derive(Clone, Debug)]
pub struct Rule {
    pub path: PathBuf,
    pub access: Access,
    pub dir: Option<Arc<File>>,
}

impl Rule {
    /// The real path the sandbox matches: a directory's is the one its
    /// descriptor holds, however `path` has moved since.
    fn real_path(&self) -> io::Result<PathBuf> {
        match &self.dir {
            Some(dir) => backend::descriptor_path(dir),
            None => std::fs::canonicalize(&self.path),
        }
    }
}

/// Everything a child may reach: the baseline, the guest's grants, and the
/// network all or none.
#[derive(Clone, Debug)]
pub struct Policy {
    rules: Vec<Rule>,
    network: Network,
}

impl Policy {
    /// The baseline alone: what a native program needs to load and run.
    #[must_use]
    pub fn new(network: Network) -> Self {
        let mut policy = Self {
            rules: Vec::new(),
            network,
        };
        for path in BASELINE_READ {
            policy.grant_path(path, Access::ReadExecute);
        }
        policy.grant_path("/dev/null", Access::ReadWrite);
        policy
    }

    /// A directory the guest was granted, through the descriptor its preopen
    /// holds.
    pub fn grant_dir(&mut self, path: impl Into<PathBuf>, dir: Arc<File>) {
        self.rules.push(Rule {
            path: path.into(),
            access: Access::ReadWrite,
            dir: Some(dir),
        });
    }

    /// A path the host names itself: a private `HOME`, a toolchain the user
    /// adds to the baseline.
    pub fn grant_path(&mut self, path: impl Into<PathBuf>, access: Access) {
        self.rules.push(Rule {
            path: path.into(),
            access,
            dir: None,
        });
    }

    /// The controlling terminal, for a child that inherits a stream from it.
    pub fn grant_terminal(&mut self) {
        self.grant_path("/dev/tty", Access::ReadWrite);
    }

    /// The rules at the real paths they name, as the sandbox matches them.
    #[must_use]
    pub fn resolved(&self) -> ResolvedPolicy {
        ResolvedPolicy(
            self.rules
                .iter()
                .filter_map(|rule| Some((rule.real_path().ok()?, rule.access)))
                .collect(),
        )
    }

    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }
}

/// A [`Policy`]'s rules at their real paths, a rule naming no file left out.
#[derive(Clone, Debug)]
pub struct ResolvedPolicy(Vec<(PathBuf, Access)>);

impl ResolvedPolicy {
    /// Whether a rule grants `access` to `path`, itself a real path.
    #[must_use]
    pub fn permits(&self, path: &Path, access: Access) -> bool {
        self.0
            .iter()
            .any(|(root, granted)| path.starts_with(root) && *granted >= access)
    }

    /// The first interpreter that `exec` of `program`, a real path, opens and
    /// the policy does not let the child execute. Landlock refuses the `exec`
    /// there even where `program` itself is granted.
    #[cfg(unix)]
    #[must_use]
    pub fn refused_interpreter(&self, program: &Path) -> Option<PathBuf> {
        exec::interpreters(program)
            .find(|interpreter| !self.permits(interpreter, Access::ReadExecute))
    }

    /// What Linux grants the child `pid` beyond its policy: its own `/proc`
    /// entry.
    #[cfg(target_os = "linux")]
    fn grant_own_proc(&mut self, pid: libc::pid_t) {
        self.0
            .push((PathBuf::from(format!("/proc/{pid}")), Access::ReadExecute));
    }
}

/// Read and execute: the loader, the shared libraries, installed programs, the
/// system configuration (`/etc`: time zones, name resolution, TLS certificates,
/// a toolchain's settings), and the randomness devices. A path absent on this
/// machine is skipped. Fixed per OS: nothing here comes from the program's
/// path, which the guest chooses.
#[cfg(target_os = "linux")]
const BASELINE_READ: &[&str] = &[
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/usr",
    "/nix/store",
    "/etc",
    "/dev/urandom",
    "/dev/random",
    "/dev/zero",
    // What the system says of itself, which every user reads. A process's
    // own entry is granted per child, never another's.
    "/proc/cpuinfo",
    "/proc/meminfo",
    "/proc/stat",
    "/proc/loadavg",
    "/proc/uptime",
    "/proc/version",
    "/proc/filesystems",
    "/proc/sys",
];

#[cfg(target_os = "macos")]
const BASELINE_READ: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/opt/homebrew",
    "/opt/local",
    // The Command Line Tools the `/usr/bin` stubs (`git`, `cc`, `python3`)
    // run, and the Xcode license acceptance they check first. Not
    // `/Applications/Xcode.app`: an admin user, who need not be root, can
    // replace it with a link to anywhere.
    "/Library/Developer/CommandLineTools",
    "/Library/Preferences/com.apple.dt.Xcode.plist",
    "/private/etc",
    // Which shell `/bin/sh` runs.
    "/private/var/select",
    "/private/var/db/timezone",
    "/dev/urandom",
    "/dev/random",
    "/dev/zero",
];

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const BASELINE_READ: &[&str] = &[];

/// Where one of the child's standard streams goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stdio {
    Null,
    Inherit,
    Piped,
}

/// One confined spawn.
pub struct Command<'a> {
    /// An absolute path: the caller resolves a bare name on `PATH`.
    pub program: &'a Path,
    pub args: &'a [OsString],
    /// The child's whole environment; nothing of the host's passes on.
    pub env: &'a [(OsString, OsString)],
    /// The directory the child starts in, as the descriptor the host holds.
    pub cwd: Option<&'a File>,
    pub stdin: Stdio,
    pub stdout: Stdio,
    pub stderr: Stdio,
    pub policy: &'a Policy,
    pub trace: Trace,
}

/// What the host watches a child do, beyond the calls the filter refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trace {
    Off,
    /// Each call that names a path the sandbox mediates, recording a path the
    /// policy does not grant as a [`Refusal::Path`]. Every such call waits on
    /// the host, so a child runs slower. Linux alone watches.
    Files,
}

/// A child that started, with the host's ends of its piped streams.
pub struct Spawned {
    pub child: Child,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
    pub refusals: Refusals,
    /// Why `refusals` stays empty whatever the sandbox refuses the child,
    /// where it does.
    pub unobserved: Option<&'static str>,
}

/// What the sandbox refused a child. Linux alone reports one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refusal {
    /// A path a call names, at its real path, asking `access` the policy does
    /// not grant. `dir` is the directory a grant must cover: the one holding
    /// the file opened or the entry made, removed or moved, or the directory
    /// opened itself. Seen under [`Trace::Files`]. Landlock refuses the call,
    /// which the child sees fail with `EACCES`.
    Path {
        path: PathBuf,
        dir: PathBuf,
        access: Access,
    },
    /// A path in a process's `/proc` entry: of any process but the child,
    /// a descendant's own included, or one the child writes, its own being
    /// granted for reading alone. Seen under
    /// [`Trace::Files`], and named without the pid, which no grant can name
    /// ahead of the process.
    ProcessEntry {
        access: Access,
    },
    // The rest are calls the filter refuses, which the child sees fail with
    // `EPERM`.
    /// A socket, with the network not granted.
    Network,
    /// A UNIX socket, which a path can connect to a service outside.
    UnixSocket,
    /// `setsid` or `setpgid`, out of the group the host kills.
    LeaveGroup,
    Trace,
    /// `io_uring`, whose operations the filter never sees.
    IoUring,
    /// `TIOCSTI` or `TIOCLINUX`, typing into the terminal.
    TerminalInput,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Path {
                path,
                access: Access::ReadExecute,
                ..
            } => write!(f, "reading {}", path.display()),
            Refusal::Path {
                path,
                access: Access::ReadWrite,
                ..
            } => write!(f, "writing {}", path.display()),
            Refusal::ProcessEntry {
                access: Access::ReadExecute,
            } => f.write_str("reading a /proc entry other than the child's own"),
            Refusal::ProcessEntry {
                access: Access::ReadWrite,
            } => f.write_str("writing a process's /proc entry, which no grant allows"),
            Refusal::Network => f.write_str("a network socket, with no network granted"),
            Refusal::UnixSocket => {
                f.write_str("a UNIX socket, which can reach a service outside the sandbox")
            }
            Refusal::LeaveGroup => f.write_str("leaving its process group"),
            Refusal::Trace => f.write_str("tracing another process"),
            Refusal::IoUring => {
                f.write_str("io_uring, whose operations the sandbox cannot inspect")
            }
            Refusal::TerminalInput => f.write_str("typing into a terminal"),
        }
    }
}

/// What a child's group has been refused so far. A refused call blocks until
/// it is recorded, so once [`Child::wait`] returns, the set holds every refusal
/// the child itself met; a descendant that outlives it may add more.
#[derive(Clone, Default)]
pub struct Refusals(Arc<Mutex<Seen>>);

#[derive(Default)]
struct Seen {
    refused: BTreeSet<Refusal>,
    /// A file call whose path the host may not read, so whether Landlock
    /// refused it goes unsaid.
    unseen: bool,
}

impl Refusals {
    #[must_use]
    pub fn collect(&self) -> BTreeSet<Refusal> {
        lock(&self.0).refused.clone()
    }

    /// Why [`Trace::Files`] missed a file call, where it did.
    #[must_use]
    pub fn unseen(&self) -> Option<&'static str> {
        lock(&self.0).unseen.then_some(
            "the host may not read the path of a file call (Yama's `ptrace_scope`, or a \
             process that made itself non-dumpable)",
        )
    }

    #[cfg(target_os = "linux")]
    fn record(&self, refusal: Refusal) {
        lock(&self.0).refused.insert(refusal);
    }

    #[cfg(target_os = "linux")]
    fn record_unseen(&self) {
        lock(&self.0).unseen = true;
    }
}

/// Start `command` confined to its policy, in a process group of its own,
/// holding descriptors 0, 1 and 2 and no other.
///
/// # Errors
///
/// Returns the OS error that kept the child from starting, `ENOENT` for a
/// program that does not exist and `EACCES` for one the sandbox refuses.
///
/// # Panics
///
/// Panics when [`availability`] is not `Available`: a host offers spawning only
/// where it can confine.
pub fn spawn(command: &Command<'_>) -> io::Result<Spawned> {
    assert_eq!(
        availability(),
        Availability::Available,
        "spawning requires a host that can confine a child"
    );
    assert!(
        command.program.is_absolute(),
        "the program must be resolved to an absolute path"
    );
    backend::spawn(command, terminal_for(command))
}

/// Whether a child takes the host's terminal for as long as it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Terminal {
    /// The child inherits no stream from a terminal the host holds.
    Kept,
    /// The child's group is the foreground group of the terminal on `fd`, as
    /// a shell makes the job it runs.
    HandedOver(RawFd),
}

/// A child of an unconfined spawn shares its parent's group, and with it the
/// parent's place on the terminal. A child in a group of its own reads and
/// sets the terminal only as the foreground group, so the host hands over the
/// terminal it holds where the child inherits a stream from it.
fn terminal_for(command: &Command<'_>) -> Terminal {
    [(0, command.stdin), (1, command.stdout), (2, command.stderr)]
        .into_iter()
        .filter(|&(_, stdio)| stdio == Stdio::Inherit)
        // SAFETY: plain system calls.
        .find(|&(fd, _)| unsafe { libc::isatty(fd) == 1 } && terminal_group(fd) == own_group())
        .map_or(Terminal::Kept, |(fd, _)| Terminal::HandedOver(fd))
}

/// Make `group` the foreground group of the terminal on `fd`. A group in the
/// background may do so only with `SIGTTOU` blocked, as a shell does.
/// Async-signal-safe, so a child may call it between `fork` and `exec`.
fn give_terminal(fd: RawFd, group: libc::pid_t) -> io::Result<()> {
    // SAFETY: plain calls on signal sets that live on this stack.
    unsafe {
        let mut ttou: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut ttou);
        libc::sigaddset(&raw mut ttou, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const ttou, &raw mut old);
        let ret = libc::tcsetpgrp(fd, group);
        let err = io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
        if ret == -1 { Err(err) } else { Ok(()) }
    }
}

/// The foreground group of the terminal on `fd`.
fn terminal_group(fd: RawFd) -> libc::pid_t {
    // SAFETY: plain system call.
    unsafe { libc::tcgetpgrp(fd) }
}

fn own_group() -> libc::pid_t {
    // SAFETY: plain system call.
    unsafe { libc::getpgrp() }
}

/// The path the OS holds for an open directory, where a relative program path
/// resolves.
///
/// # Errors
///
/// Returns the OS error the lookup reports.
///
/// # Panics
///
/// Panics when [`availability`] is not `Available`.
pub fn descriptor_path(file: &File) -> io::Result<PathBuf> {
    assert_eq!(
        availability(),
        Availability::Available,
        "only a host that can confine a child spawns"
    );
    backend::descriptor_path(file)
}

/// How a child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    Exited(u8),
    Signaled(u8),
}

/// A child process the host has not reaped yet, and the group it leads.
///
/// The group is signalled only before its leader is reaped: until then the
/// leader holds its pid, so the group's id cannot pass to an unrelated process.
pub struct Child {
    pid: libc::pid_t,
    terminal: Terminal,
    reaped: Mutex<bool>,
}

impl Child {
    fn new(pid: libc::pid_t, terminal: Terminal) -> Self {
        Self {
            pid,
            terminal,
            reaped: Mutex::new(false),
        }
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// Send `signal` to the child's whole process group. A no-op once the
    /// child is reaped.
    ///
    /// # Errors
    ///
    /// Returns the OS error `killpg` reports.
    pub fn signal_group(&self, signal: libc::c_int) -> io::Result<()> {
        let reaped = lock(&self.reaped);
        if *reaped {
            return Ok(());
        }
        // SAFETY: plain system call; the unreaped leader keeps the group id ours.
        if unsafe { libc::killpg(self.pid, signal) } == -1 {
            let err = io::Error::last_os_error();
            // The leader exited and every member is gone; nothing to signal.
            // Darwin answers EPERM instead when the group holds only zombies,
            // as it does while `wait` keeps the leader unreaped.
            let nothing_to_signal = err.raw_os_error() == Some(libc::ESRCH)
                || (cfg!(target_os = "macos") && err.raw_os_error() == Some(libc::EPERM));
            if nothing_to_signal {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    /// Block until the child exits, kill what is left of its group, then reap
    /// it. A descendant never outlives the child, so none keeps its grants
    /// unwatched.
    ///
    /// # Errors
    ///
    /// Returns the OS error `waitid` or `waitpid` reports.
    ///
    /// # Panics
    ///
    /// Panics when called a second time: a child is reaped once.
    pub fn wait(&self) -> io::Result<ExitStatus> {
        // Wait without reaping, so a `signal_group` racing the exit still finds
        // the leader holding the group id.
        match self.terminal {
            Terminal::Kept => {
                self.wait_id(libc::WEXITED | libc::WNOWAIT)?;
            }
            // A child holding the terminal stops on its own, at Ctrl-Z, and
            // the host follows it.
            Terminal::HandedOver(fd) => {
                let events = libc::WEXITED | libc::WSTOPPED | libc::WNOWAIT;
                while self.wait_id(events)? == libc::CLD_STOPPED {
                    self.wait_id(libc::WSTOPPED)?;
                    self.follow_stop(fd)?;
                }
                if terminal_group(fd) == self.pid {
                    give_terminal(fd, own_group())?;
                }
            }
        }
        self.signal_group(libc::SIGKILL)?;
        let mut reaped = lock(&self.reaped);
        assert!(!*reaped, "a child is reaped once");
        let mut status = 0;
        // SAFETY: plain system call; the child has exited, so this returns at once.
        if unsafe { libc::waitpid(self.pid, &raw mut status, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        *reaped = true;
        if libc::WIFEXITED(status) {
            Ok(ExitStatus::Exited(libc::WEXITSTATUS(status) as u8))
        } else {
            Ok(ExitStatus::Signaled(libc::WTERMSIG(status) as u8))
        }
    }

    /// The `si_code` of the child's next event among `options`.
    fn wait_id(&self, options: libc::c_int) -> io::Result<libc::c_int> {
        loop {
            // SAFETY: `info` is plain data the kernel fills.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: plain system call on our own child.
            let ret = unsafe {
                libc::waitid(libc::P_PID, self.pid as libc::id_t, &raw mut info, options)
            };
            if ret == 0 {
                return Ok(info.si_code);
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err);
            }
        }
    }

    /// The child holding the terminal stopped, as a job does at Ctrl-Z. Sharing
    /// its group, the host would stop with it, so it does: the terminal goes
    /// back to the host's group, which stops. Continued in the foreground, the
    /// host hands the terminal over again, and the child continues.
    fn follow_stop(&self, fd: RawFd) -> io::Result<()> {
        if terminal_group(fd) == self.pid {
            give_terminal(fd, own_group())?;
        }
        // SAFETY: plain system call; it returns once the group is continued.
        if unsafe { libc::killpg(own_group(), libc::SIGTSTP) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if terminal_group(fd) == own_group() {
            give_terminal(fd, self.pid)?;
        }
        self.signal_group(libc::SIGCONT)
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
