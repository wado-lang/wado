//! The sandbox a spawned child runs in: confined to the guest's grants plus a
//! fixed system baseline, never more. See WEP 2026-10-21 (`core:process`).
//!
//! The parent stays unconfined. Each backend confines the child alone, without
//! re-executing `wado`, so a host embedded in another binary spawns as `wado`
//! does.

use std::ffi::OsString;
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

/// What a child may do beneath one subtree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        if network == Network::Granted {
            for path in BASELINE_NETWORK_READ {
                policy.grant_path(path, Access::ReadExecute);
            }
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

    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }
}

/// Read and execute: the loader, the shared libraries, installed programs,
/// locale data and time zones, and the randomness devices. A path absent on
/// this machine is skipped. Fixed per OS: nothing here comes from the program's
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
    "/etc/ld.so.cache",
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
    "/dev/urandom",
    "/dev/random",
    "/dev/zero",
];

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const BASELINE_READ: &[&str] = &[];

/// Name resolution and TLS certificates, read only with the network granted.
#[cfg(target_os = "linux")]
const BASELINE_NETWORK_READ: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/ssl",
    "/etc/pki",
];

#[cfg(target_os = "macos")]
const BASELINE_NETWORK_READ: &[&str] = &[
    "/private/etc/resolv.conf",
    "/private/etc/hosts",
    "/private/etc/ssl",
    "/private/var/run/mDNSResponder",
];

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const BASELINE_NETWORK_READ: &[&str] = &[];

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
}

/// A child that started, with the host's ends of its piped streams.
pub struct Spawned {
    pub child: Child,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
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
    assert!(command.program.is_absolute(), "the program must be resolved to an absolute path");
    backend::spawn(command)
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
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    /// Block until the child exits, then reap it.
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

#[cfg(all(test, target_os = "linux"))]
mod tests;
