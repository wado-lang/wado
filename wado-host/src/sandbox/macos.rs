//! macOS confinement through Seatbelt. `sandbox_init` compiles its profile and
//! allocates, which a multithreaded process may not do between `fork` and
//! `exec`, so the child starts as `/usr/bin/sandbox-exec`, a single-threaded
//! process of its own, spawned with `posix_spawn`, which does not fork.
//!
//! The profile's text holds no path. Each path reaches it as a parameter, so no
//! path can rewrite the policy, however it is spelled.

use std::collections::BTreeSet;
use std::ffi::{CString, OsStr, OsString};
use std::fmt::Write as _;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use super::{
    Access, Availability, Child, Command, Network, Policy, Refusals, Spawned, Stdio, Terminal,
    give_terminal,
};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

unsafe extern "C" {
    /// Keeps `fd` open across a spawn under `POSIX_SPAWN_CLOEXEC_DEFAULT`.
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: libc::c_int,
    ) -> libc::c_int;
}

pub(super) fn availability() -> Availability {
    static ANSWER: OnceLock<Availability> = OnceLock::new();
    ANSWER
        .get_or_init(|| {
            if Path::new(SANDBOX_EXEC).exists() {
                Availability::Available
            } else {
                Availability::Unsupported(format!("{SANDBOX_EXEC} is missing"))
            }
        })
        .clone()
}

/// The fixed part of the profile. Everything a rule grants is appended after
/// it, one rule per parameter.
const PROFILE_BASE: &str = r#"(version 1)
(deny default)
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
; dyld lists `/` and stats the `/var` link while it finds its shared cache, and
; aborts when it cannot. `/etc`, `/tmp` and `/var` link into `/private`, where a
; grant is matched, and a path is resolved through the link only where the link
; itself can be read.
(allow file-read-data (literal "/"))
(allow file-read-metadata (literal "/etc") (literal "/tmp") (literal "/var"))
; A descriptor the child already holds, opened again by its number: no access
; it lacks.
(allow file-read* file-write-data (subpath "/dev/fd"))
(allow file-read-metadata (literal "/dev/stdin") (literal "/dev/stdout") (literal "/dev/stderr"))
; Where the `/usr/bin` developer tool stubs find the developer directory
; `xcode-select` chose. Root sets the link; the directory it names is a grant
; of its own.
(allow file-read-metadata (literal "/private/var/db/xcode_select_link"))
(allow mach-lookup
    (global-name "com.apple.system.opendirectoryd.libinfo")
    (global-name "com.apple.system.notification_center")
    (global-name "com.apple.system.logger")
    (global-name "com.apple.logd"))
"#;

/// The rules for a network grant: IP sockets, the one UNIX socket by path name
/// resolution goes through, and the services that decide whether to trust a
/// TLS certificate, which may fetch over the network for the child.
const PROFILE_NETWORK: &str = r#"(allow system-socket)
(allow network-outbound (remote ip))
(allow network-inbound (local ip))
(allow network-bind (local ip))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
(allow mach-lookup
    (global-name "com.apple.trustd.agent")
    (global-name "com.apple.TrustEvaluationAgent"))
"#;

/// The profile for `policy`, and the parameters it reads. Seatbelt matches the
/// real path, so each parameter is one: `/tmp` would never match
/// `/private/tmp`.
fn profile(policy: &Policy) -> io::Result<(String, Vec<OsString>)> {
    let mut text = PROFILE_BASE.to_owned();
    let mut params = Vec::new();
    // Resolving a path stats each directory above it and reads each link on
    // the way, so a grant's ancestors and those links show their metadata and
    // nothing else.
    let mut traversed = BTreeSet::new();
    for rule in policy.rules() {
        let real = match rule.real_path() {
            Ok(real) => real,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(io::Error::new(
                    err.kind(),
                    format!("{}: {err}", rule.path.display()),
                ));
            }
        };
        let operations = match rule.access {
            Access::ReadExecute => "file-read* process-exec",
            Access::ReadWrite => "file-read* file-write* process-exec",
        };
        allow(&mut text, &mut params, operations, "subpath", &real);
        traversed.extend(real.ancestors().skip(1).map(Path::to_path_buf));
        traversed.extend(links_on(&rule.path));
    }
    for path in &traversed {
        allow(
            &mut text,
            &mut params,
            "file-read-metadata",
            "literal",
            path,
        );
    }
    // What a program that inherits a terminal does with it, as a child of an
    // unconfined spawn can: set its mode, and ask its line discipline and
    // foreground group. `TIOCSTI` stays refused.
    writeln!(
        text,
        "(allow file-ioctl (ioctl-command #x{:x} #x{:x} #x{:x} #x{:x} #x{:x}))",
        libc::TIOCSETA,
        libc::TIOCSETAW,
        libc::TIOCSETAF,
        libc::TIOCGETD,
        libc::TIOCGPGRP,
    )
    .expect("writing to a String cannot fail");
    if policy.network() == Network::Granted {
        text.push_str(PROFILE_NETWORK);
    }
    Ok((text, params))
}

/// Darwin's `MAXSYMLINKS`: how many links one lookup follows before `ELOOP`.
const SYMLINKS_MAX: usize = 32;

/// Each symbolic link that resolving `path` reads, at the real path Seatbelt
/// matches it by: those `path` names, and those its links lead through.
fn links_on(path: &Path) -> Vec<PathBuf> {
    let mut links = Vec::new();
    let mut resolved = PathBuf::from("/");
    // The names still to resolve, the next one last.
    let mut pending: Vec<OsString> = names_reversed(path).collect();
    while let Some(name) = pending.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let next = resolved.join(&name);
        let Ok(target) = std::fs::read_link(&next) else {
            resolved = next;
            continue;
        };
        if links.len() == SYMLINKS_MAX {
            break;
        }
        links.push(next);
        if target.is_absolute() {
            resolved = PathBuf::from("/");
        }
        pending.extend(names_reversed(&target));
    }
    links
}

/// The names `path` steps through, `..` among them, from the last.
fn names_reversed(path: &Path) -> impl Iterator<Item = OsString> + '_ {
    path.components()
        .rev()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
}

/// Appends a rule allowing `operations` on `path`, which reaches the profile as
/// a parameter of its own.
fn allow(
    text: &mut String,
    params: &mut Vec<OsString>,
    operations: &str,
    filter: &str,
    path: &Path,
) {
    let name = format!("P{}", params.len());
    writeln!(text, "(allow {operations} ({filter} (param \"{name}\")))")
        .expect("writing to a String cannot fail");
    let mut param = OsString::from(format!("{name}="));
    param.push(path.as_os_str());
    params.push(param);
}

/// The real path of the file `file` names, as the kernel records it.
pub(super) fn descriptor_path(file: &File) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes, NUL-terminated.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let len = buf
        .iter()
        .position(|&b| b == 0)
        .expect("F_GETPATH terminates the path");
    buf.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

fn c_string(bytes: &OsStr) -> io::Result<CString> {
    CString::new(bytes.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "an argument holds a NUL byte"))
}

pub(super) fn spawn(command: &Command<'_>, terminal: Terminal) -> io::Result<Spawned> {
    // `sandbox-exec` reports a missing program only on its own stderr, so the
    // host asks first.
    std::fs::metadata(command.program)?;
    let (text, params) = profile(command.policy)?;

    let mut argv: Vec<CString> = vec![
        c_string(OsStr::new(SANDBOX_EXEC))?,
        c_string(OsStr::new("-p"))?,
        c_string(OsStr::new(&text))?,
    ];
    for param in &params {
        argv.push(c_string(OsStr::new("-D"))?);
        argv.push(c_string(param)?);
    }
    argv.push(c_string(command.program.as_os_str())?);
    for arg in command.args {
        argv.push(c_string(arg)?);
    }
    let envp: Vec<CString> = command
        .env
        .iter()
        .map(|(key, value)| {
            let mut pair = key.clone();
            pair.push("=");
            pair.push(value);
            c_string(&pair)
        })
        .collect::<io::Result<_>>()?;
    let argv_ptrs = null_terminated(&argv);
    let envp_ptrs = null_terminated(&envp);

    let mut actions = FileActions::new()?;
    let mut parent_ends: [Option<OwnedFd>; 3] = [None, None, None];
    // The child's ends, closed in the parent once the spawn has dup'd them.
    let mut child_ends: Vec<OwnedFd> = Vec::new();
    for (fd, stdio) in [(0, command.stdin), (1, command.stdout), (2, command.stderr)] {
        match stdio {
            Stdio::Null => actions.open(fd, c"/dev/null", libc::O_RDWR)?,
            Stdio::Inherit => actions.inherit(fd)?,
            Stdio::Piped => {
                let [read, write] = pipe()?;
                let (child, parent) = if fd == 0 {
                    (read, write)
                } else {
                    (write, read)
                };
                actions.dup2(child.as_raw_fd(), fd)?;
                child_ends.push(child);
                parent_ends[fd as usize] = Some(parent);
            }
        }
    }
    if let Some(dir) = command.cwd {
        actions.fchdir(dir.as_raw_fd())?;
    }

    let attr = SpawnAttr::new(terminal)?;
    let mut pid: libc::pid_t = 0;
    let program = c_string(OsStr::new(SANDBOX_EXEC))?;
    // SAFETY: every pointer is to memory alive for the call, the arrays
    // NUL-terminated.
    let ret = unsafe {
        libc::posix_spawn(
            &raw mut pid,
            program.as_ptr(),
            &raw const actions.0,
            &raw const attr.0,
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
        )
    };
    drop(child_ends);
    if ret != 0 {
        return Err(io::Error::from_raw_os_error(ret));
    }
    let child = Child::new(pid, terminal);
    if let Terminal::HandedOver(fd) = terminal {
        // Where the terminal refuses, the child runs in the background, as
        // under a host that does not hold it.
        let _ = give_terminal(fd, pid);
        child.signal_group(libc::SIGCONT)?;
    }
    let [stdin, stdout, stderr] = parent_ends;
    Ok(Spawned {
        child,
        stdin,
        stdout,
        stderr,
        refusals: Refusals::default(),
        unobserved: Some("Seatbelt reports nothing it refuses to the host"),
    })
}

fn null_terminated(strings: &[CString]) -> Vec<*mut libc::c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr().cast_mut())
        .chain(std::iter::once(ptr::null_mut()))
        .collect()
}

/// A pipe whose two ends are both close-on-exec: the file actions place the
/// child's end, which clears the flag on the copy.
fn pipe() -> io::Result<[OwnedFd; 2]> {
    let mut fds: [RawFd; 2] = [0; 2];
    // SAFETY: `pipe` writes two descriptors into `fds`.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just returned these descriptors to us alone.
    let ends = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    for end in &ends {
        // SAFETY: plain system call on a descriptor we own.
        if unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(ends)
}

fn check(ret: libc::c_int) -> io::Result<()> {
    if ret != 0 {
        return Err(io::Error::from_raw_os_error(ret));
    }
    Ok(())
}

struct FileActions(libc::posix_spawn_file_actions_t);

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut actions = MaybeUninit::uninit();
        // SAFETY: init fills the value it is handed.
        check(unsafe { libc::posix_spawn_file_actions_init(actions.as_mut_ptr()) })?;
        // SAFETY: initialized just above.
        Ok(Self(unsafe { actions.assume_init() }))
    }

    fn open(&mut self, fd: RawFd, path: &std::ffi::CStr, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: the action copies `path`.
        check(unsafe {
            libc::posix_spawn_file_actions_addopen(&raw mut self.0, fd, path.as_ptr(), flags, 0)
        })
    }

    fn inherit(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: plain call on initialized actions.
        check(unsafe { posix_spawn_file_actions_addinherit_np(&raw mut self.0, fd) })
    }

    fn dup2(&mut self, from: RawFd, to: RawFd) -> io::Result<()> {
        // SAFETY: plain call on initialized actions.
        check(unsafe { libc::posix_spawn_file_actions_adddup2(&raw mut self.0, from, to) })
    }

    fn fchdir(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: plain call on initialized actions.
        check(unsafe { libc::posix_spawn_file_actions_addfchdir_np(&raw mut self.0, fd) })
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: destroys what `new` initialized.
        unsafe { libc::posix_spawn_file_actions_destroy(&raw mut self.0) };
    }
}

/// A process group of its own, default signal handling, an empty mask, and
/// every descriptor past the file actions closed. A child that takes the
/// terminal starts suspended, until its group holds it: a read before then
/// would find it in the background.
struct SpawnAttr(libc::posix_spawnattr_t);

impl SpawnAttr {
    fn new(terminal: Terminal) -> io::Result<Self> {
        let mut attr = MaybeUninit::uninit();
        // SAFETY: init fills the value it is handed.
        check(unsafe { libc::posix_spawnattr_init(attr.as_mut_ptr()) })?;
        // SAFETY: initialized just above.
        let mut attr = Self(unsafe { attr.assume_init() });
        let suspended = match terminal {
            Terminal::Kept => 0,
            Terminal::HandedOver(_) => libc::POSIX_SPAWN_START_SUSPENDED,
        };
        let flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
            | libc::POSIX_SPAWN_SETPGROUP
            | libc::POSIX_SPAWN_SETSIGDEF
            | libc::POSIX_SPAWN_SETSIGMASK
            | suspended;
        // SAFETY: each call below sets one field of the initialized attributes.
        check(unsafe { libc::posix_spawnattr_setflags(&raw mut attr.0, flags as libc::c_short) })?;
        check(unsafe { libc::posix_spawnattr_setpgroup(&raw mut attr.0, 0) })?;
        let mut all: libc::sigset_t = 0;
        let mut none: libc::sigset_t = 0;
        unsafe {
            libc::sigfillset(&raw mut all);
            libc::sigemptyset(&raw mut none);
        }
        check(unsafe { libc::posix_spawnattr_setsigdefault(&raw mut attr.0, &raw const all) })?;
        check(unsafe { libc::posix_spawnattr_setsigmask(&raw mut attr.0, &raw const none) })?;
        Ok(attr)
    }
}

impl Drop for SpawnAttr {
    fn drop(&mut self) {
        // SAFETY: destroys what `new` initialized.
        unsafe { libc::posix_spawnattr_destroy(&raw mut self.0) };
    }
}
