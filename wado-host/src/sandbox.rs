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
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::sync::lock;

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

    /// Whether a child may execute `program`: what it resolves to lies beneath
    /// a rule, as the sandbox resolves both. Every rule grants execute.
    #[must_use]
    pub fn executes(&self, program: &Path) -> bool {
        std::fs::canonicalize(program)
            .is_ok_and(|program| self.resolved().permits(&program, Access::ReadExecute))
    }

    /// The rules at the real paths they name, as the sandbox matches them.
    #[must_use]
    pub fn resolved(&self) -> ResolvedPolicy {
        ResolvedPolicy(
            self.rules
                .iter()
                .filter_map(|rule| Some((std::fs::canonicalize(&rule.path).ok()?, rule.access)))
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
pub struct ResolvedPolicy(Vec<(PathBuf, Access)>);

impl ResolvedPolicy {
    /// Whether a rule grants `access` to `path`, itself a real path.
    #[must_use]
    pub fn permits(&self, path: &Path, access: Access) -> bool {
        self.0
            .iter()
            .any(|(root, granted)| path.starts_with(root) && *granted >= access)
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
];

#[cfg(target_os = "macos")]
const BASELINE_READ: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/opt/homebrew",
    "/opt/local",
    "/private/etc",
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
}

/// What the sandbox refused a child. Linux alone reports one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refusal {
    /// A path the policy does not grant `access` to, at its real path, seen
    /// under [`Trace::Files`]. Landlock refuses the call, which the child sees
    /// fail with `EACCES`.
    Path {
        path: PathBuf,
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
        f.write_str(match self {
            Refusal::Path {
                path,
                access: Access::ReadExecute,
            } => return write!(f, "reading {}", path.display()),
            Refusal::Path {
                path,
                access: Access::ReadWrite,
            } => return write!(f, "writing {}", path.display()),
            Refusal::Network => "a network socket, with no network granted",
            Refusal::UnixSocket => "a UNIX socket, which can reach a service outside the sandbox",
            Refusal::LeaveGroup => "leaving its process group",
            Refusal::Trace => "tracing another process",
            Refusal::IoUring => "io_uring, whose operations the sandbox cannot inspect",
            Refusal::TerminalInput => "typing into a terminal",
        })
    }
}

/// What a child's group has been refused so far. A refused call blocks until
/// it is recorded, so once [`Child::wait`] returns, the set holds every refusal
/// the child itself met; a descendant that outlives it may add more.
#[derive(Clone, Default)]
pub struct Refusals(Arc<Mutex<BTreeSet<Refusal>>>);

impl Refusals {
    #[must_use]
    pub fn collect(&self) -> BTreeSet<Refusal> {
        lock(&self.0).clone()
    }

    fn record(&self, refusal: Refusal) {
        lock(&self.0).insert(refusal);
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
    backend::spawn(command)
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
    reaped: Mutex<bool>,
}

impl Child {
    fn new(pid: libc::pid_t) -> Self {
        Self {
            pid,
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
            let nothing_to_signal = match err.raw_os_error() {
                Some(libc::ESRCH) => true,
                Some(libc::EPERM) => cfg!(target_os = "macos"),
                _ => false,
            };
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
        loop {
            // SAFETY: `info` is plain data the kernel fills.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: plain system call on our own child.
            let ret = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if ret == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err);
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
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
