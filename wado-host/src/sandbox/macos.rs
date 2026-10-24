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
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use super::{Access, Availability, Child, Command, Network, Policy, Spawned, Stdio};

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
; aborts when it cannot.
(allow file-read-data (literal "/"))
(allow file-read-metadata (literal "/var"))
(allow mach-lookup
    (global-name "com.apple.system.opendirectoryd.libinfo")
    (global-name "com.apple.system.notification_center")
    (global-name "com.apple.system.logger")
    (global-name "com.apple.logd"))
"#;

/// The rules for a network grant: IP sockets, and the one UNIX socket by path
/// name resolution goes through.
const PROFILE_NETWORK: &str = r#"(allow system-socket)
(allow network-outbound (remote ip))
(allow network-inbound (local ip))
(allow network-bind (local ip))
(allow network-outbound (literal "/private/var/run/mDNSResponder"))
"#;

/// The profile for `policy`, and the parameters it reads. Seatbelt matches the
/// real path, so each parameter is one: `/tmp` would never match
/// `/private/tmp`.
fn profile(policy: &Policy) -> io::Result<(String, Vec<OsString>)> {
    let mut text = PROFILE_BASE.to_owned();
    let mut params = Vec::new();
    // Resolving a path stats each directory above it, so a grant's ancestors
    // show their metadata and nothing else.
    let mut ancestors = BTreeSet::new();
    for rule in policy.rules() {
        let real = match &rule.dir {
            Some(dir) => descriptor_path(dir)?,
            None => match std::fs::canonicalize(&rule.path) {
                Ok(real) => real,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(io::Error::new(
                        err.kind(),
                        format!("{}: {err}", rule.path.display()),
                    ));
                }
            },
        };
        let operations = match rule.access {
            Access::ReadExecute => "file-read* process-exec",
            Access::ReadWrite => "file-read* file-write* process-exec",
        };
        allow(&mut text, &mut params, operations, "subpath", &real);
        ancestors.extend(real.ancestors().skip(1).map(Path::to_path_buf));
    }
    for ancestor in &ancestors {
        allow(
            &mut text,
            &mut params,
            "file-read-metadata",
            "literal",
            ancestor,
        );
    }
    if policy.network() == Network::Granted {
        text.push_str(PROFILE_NETWORK);
    }
    Ok((text, params))
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

pub(super) fn spawn(command: &Command<'_>) -> io::Result<Spawned> {
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

    let attr = SpawnAttr::new()?;
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
    let [stdin, stdout, stderr] = parent_ends;
    Ok(Spawned {
        child: Child::new(pid),
        stdin,
        stdout,
        stderr,
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
/// every descriptor past the file actions closed.
struct SpawnAttr(libc::posix_spawnattr_t);

impl SpawnAttr {
    fn new() -> io::Result<Self> {
        let mut attr = MaybeUninit::uninit();
        // SAFETY: init fills the value it is handed.
        check(unsafe { libc::posix_spawnattr_init(attr.as_mut_ptr()) })?;
        // SAFETY: initialized just above.
        let mut attr = Self(unsafe { attr.assume_init() });
        let flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
            | libc::POSIX_SPAWN_SETPGROUP
            | libc::POSIX_SPAWN_SETSIGDEF
            | libc::POSIX_SPAWN_SETSIGMASK;
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
