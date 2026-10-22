//! Confines a child process to a guest's grants plus a fixed system baseline.
//! See WEP 2026-10-21 (`core:process`).
//!
//! Only Linux confines: Landlock decides what the child may reach on the
//! filesystem and which processes it may signal, and seccomp closes what
//! Landlock does not mediate. Elsewhere [`kernel_support`] says why not, and
//! no child is started.

use std::path::PathBuf;
use std::process::Command;

/// What a confined child may reach beyond the system baseline.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Directories the child may read, write and execute under: the guest's
    /// grants.
    pub grants: Vec<PathBuf>,
    /// The child's private `HOME` and `TMPDIR`, which it may also write.
    pub scratch: Vec<PathBuf>,
    /// Whether the child may open sockets other than a UNIX socket pair.
    pub network: bool,
    /// Whether the child may open `/dev/tty`: only when it inherits the
    /// terminal.
    pub tty: bool,
}

/// Why this host cannot confine a child, or `Ok` when it can.
///
/// # Errors
///
/// Returns the reason, worded for the user.
pub fn kernel_support() -> Result<(), String> {
    imp::kernel_support()
}

/// Arrange for `command` to start confined to `policy`, in a process group of
/// its own.
///
/// # Errors
///
/// Returns an error if a grant cannot be opened or the rules cannot be built.
///
/// # Panics
///
/// Panics where [`kernel_support`] reports this host cannot confine.
pub fn confine(command: &mut Command, policy: &Policy) -> std::io::Result<()> {
    imp::confine(command, policy)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Policy;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    /// The ABI that adds the signal and abstract-socket scopes, without which a
    /// child could reach processes outside its sandbox.
    const MIN_ABI: i64 = 6;

    const ACCESS_EXECUTE: u64 = 1 << 0;
    const ACCESS_WRITE_FILE: u64 = 1 << 1;
    const ACCESS_READ_FILE: u64 = 1 << 2;
    const ACCESS_READ_DIR: u64 = 1 << 3;
    const ACCESS_TRUNCATE: u64 = 1 << 14;
    const ACCESS_IOCTL_DEV: u64 = 1 << 15;
    /// Every filesystem right ABI 6 knows, so none passes unhandled.
    const ACCESS_ALL: u64 = (1 << 16) - 1;
    /// The rights a rule on a file (rather than a directory) may carry.
    const ACCESS_FILE: u64 =
        ACCESS_EXECUTE | ACCESS_WRITE_FILE | ACCESS_READ_FILE | ACCESS_TRUNCATE | ACCESS_IOCTL_DEV;
    const READ: u64 = ACCESS_READ_FILE | ACCESS_READ_DIR;
    const READ_EXEC: u64 = READ | ACCESS_EXECUTE;

    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const SCOPE_SIGNAL: u64 = 1 << 1;

    const CREATE_RULESET_VERSION: u32 = 1 << 0;
    const RULE_PATH_BENEATH: i32 = 1;

    /// The installed programs, the loader and its libraries, and time zones.
    const BASELINE: &[&str] = &[
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib32",
        "/lib64",
        "/libx32",
        "/nix/store",
        "/etc/ld.so.cache",
        "/etc/localtime",
    ];
    /// What name resolution and TLS read, open only with the network.
    const NETWORK_BASELINE: &[&str] = &[
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/host.conf",
        "/etc/gai.conf",
        "/etc/ssl",
        "/etc/pki",
        "/etc/ca-certificates",
    ];
    const READ_DEVICES: &[&str] = &["/dev/zero", "/dev/random", "/dev/urandom"];

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

    pub(super) fn kernel_support() -> Result<(), String> {
        // SAFETY: the version query reads no attribute.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 0 {
            return Err(format!(
                "this kernel does not enable Landlock ({})",
                io::Error::last_os_error()
            ));
        }
        if abi < MIN_ABI {
            return Err(format!(
                "this kernel offers Landlock ABI {abi}; spawning needs {MIN_ABI} (Linux 6.12)"
            ));
        }
        Ok(())
    }

    pub(super) fn confine(command: &mut Command, policy: &Policy) -> io::Result<()> {
        assert!(kernel_support().is_ok(), "confine needs a kernel that can");
        let ruleset = Arc::new(ruleset(policy)?);
        let filter = Arc::new(filter(policy.network));
        // SAFETY: the hook makes only async-signal-safe system calls, on data
        // built before the fork.
        unsafe {
            command.pre_exec(move || restrict(ruleset.as_raw_fd(), &filter));
        }
        Ok(())
    }

    /// The child's half, between `fork` and `exec`. It leaves the guest's
    /// process group first, since the filter then forbids leaving its own.
    fn restrict(ruleset: i32, filter: &[libc::sock_filter]) -> io::Result<()> {
        let program = libc::sock_fprog {
            len: u16::try_from(filter.len()).expect("the filter is short"),
            filter: filter.as_ptr().cast_mut(),
        };
        // SAFETY: each call takes plain integers or the program above, which
        // outlives it.
        let failed = unsafe {
            libc::setpgid(0, 0) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0) != 0
                || libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER,
                    &raw const program,
                ) != 0
        };
        if failed {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn ruleset(policy: &Policy) -> io::Result<OwnedFd> {
        let attr = RulesetAttr {
            handled_access_fs: ACCESS_ALL,
            handled_access_net: 0,
            scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
        };
        // SAFETY: `attr` is the size passed, and lives across the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &raw const attr,
                size_of::<RulesetAttr>(),
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel just returned this descriptor, owned by no one else.
        let ruleset = unsafe { OwnedFd::from_raw_fd(i32::try_from(fd).expect("a descriptor")) };

        for grant in policy.grants.iter().chain(&policy.scratch) {
            allow(&ruleset, grant, ACCESS_ALL)?;
        }
        let optional = BASELINE
            .iter()
            .map(|path| (*path, READ_EXEC))
            .chain(
                NETWORK_BASELINE
                    .iter()
                    .filter(|_| policy.network)
                    .map(|path| (*path, READ)),
            )
            .chain(READ_DEVICES.iter().map(|path| (*path, ACCESS_READ_FILE)))
            .chain([(
                "/dev/null",
                ACCESS_READ_FILE | ACCESS_WRITE_FILE | ACCESS_TRUNCATE,
            )])
            .chain(policy.tty.then_some((
                "/dev/tty",
                ACCESS_READ_FILE | ACCESS_WRITE_FILE | ACCESS_IOCTL_DEV,
            )));
        for (path, access) in optional {
            match allow(&ruleset, Path::new(path), access) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                other => other?,
            }
        }
        Ok(ruleset)
    }

    /// Let the child reach `path` and what lies beneath it with `access`,
    /// narrowed to the file rights when `path` is not a directory.
    fn allow(ruleset: &OwnedFd, path: &Path, access: u64) -> io::Result<()> {
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call.
        let raw = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `open` just returned this descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let access = if std::fs::metadata(path)?.is_dir() {
            access
        } else {
            access & ACCESS_FILE
        };
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: fd.as_raw_fd(),
        };
        // SAFETY: `attr` lives across the call.
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                RULE_PATH_BENEATH,
                &raw const attr,
                0,
            )
        };
        if added != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;

    const OFFSET_NR: u32 = 0;
    const OFFSET_ARCH: u32 = 4;
    /// The low 32 bits of argument `i`, on a little-endian target.
    const fn offset_arg(i: u32) -> u32 {
        16 + 8 * i
    }

    const RET_KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
    const RET_ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
    const RET_EPERM: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    const RET_ENOSYS: u32 = libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32;

    fn stmt(code: u32, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code: u16::try_from(code).expect("a BPF opcode"),
            jt: 0,
            jf: 0,
            k,
        }
    }

    fn jump(code: u32, k: u32, jt: usize, jf: usize) -> libc::sock_filter {
        libc::sock_filter {
            code: u16::try_from(code).expect("a BPF opcode"),
            jt: u8::try_from(jt).expect("a short jump"),
            jf: u8::try_from(jf).expect("a short jump"),
            k,
        }
    }

    fn load(offset: u32) -> libc::sock_filter {
        stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset)
    }

    fn ret(value: u32) -> libc::sock_filter {
        stmt(libc::BPF_RET | libc::BPF_K, value)
    }

    fn syscall_nr(nr: libc::c_long) -> u32 {
        u32::try_from(nr).expect("a syscall number")
    }

    fn word(value: libc::c_int) -> u32 {
        u32::try_from(value).expect("a non-negative constant")
    }

    /// Answer `body` for system call `nr`; any other call falls through.
    fn rule(nr: libc::c_long, body: Vec<libc::sock_filter>) -> Vec<libc::sock_filter> {
        let mut out = vec![
            load(OFFSET_NR),
            jump(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                syscall_nr(nr),
                0,
                body.len(),
            ),
        ];
        out.extend(body);
        out
    }

    /// Answer `denied` where argument `arg` (its low 32 bits, masked by
    /// `mask`) is one of `values`, and allow the call otherwise.
    fn deny_arg(arg: u32, mask: u32, values: &[u32], denied: u32) -> Vec<libc::sock_filter> {
        let mut body = vec![load(offset_arg(arg))];
        if mask != u32::MAX {
            body.push(stmt(libc::BPF_ALU | libc::BPF_AND | libc::BPF_K, mask));
        }
        for (i, value) in values.iter().enumerate() {
            // A match lands on the `denied` return, past the remaining
            // comparisons and the `allow`.
            let to_denied = values.len() - i;
            body.push(jump(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                *value,
                to_denied,
                0,
            ));
        }
        body.push(ret(RET_ALLOW));
        body.push(ret(denied));
        body
    }

    /// The seccomp program: what Landlock leaves open.
    pub(super) fn filter(network: bool) -> Vec<libc::sock_filter> {
        let mut program = vec![
            load(OFFSET_ARCH),
            jump(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                AUDIT_ARCH,
                1,
                0,
            ),
            ret(RET_KILL),
        ];
        // The x32 ABI reaches the same calls under other numbers.
        #[cfg(target_arch = "x86_64")]
        program.extend([
            load(OFFSET_NR),
            jump(
                libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K,
                0x4000_0000,
                0,
                1,
            ),
            ret(RET_KILL),
        ]);
        // Landlock restricts TCP only, and no UNIX socket named by a path.
        let socket = if network {
            deny_arg(0, u32::MAX, &[word(libc::AF_UNIX)], RET_EPERM)
        } else {
            vec![ret(RET_EPERM)]
        };
        program.extend(rule(libc::SYS_socket, socket));
        // A datagram pair can still address a socket by path.
        let sock_type_mask = 0xf;
        program.extend(rule(
            libc::SYS_socketpair,
            deny_arg(1, sock_type_mask, &[word(libc::SOCK_DGRAM)], RET_EPERM),
        ));
        // The host ends a child by signalling its process group.
        program.extend(rule(libc::SYS_setsid, vec![ret(RET_EPERM)]));
        program.extend(rule(libc::SYS_setpgid, vec![ret(RET_EPERM)]));
        // Input pushed into an inherited terminal runs in the user's shell.
        #[allow(clippy::cast_possible_truncation, reason = "ioctl numbers fit 32 bits")]
        let terminal_input = [libc::TIOCSTI as u32, libc::TIOCLINUX as u32];
        program.extend(rule(
            libc::SYS_ioctl,
            deny_arg(1, u32::MAX, &terminal_input, RET_EPERM),
        ));
        program.extend(rule(libc::SYS_ptrace, vec![ret(RET_EPERM)]));
        // io_uring opens sockets without the `socket` call the rules above see.
        for nr in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            program.extend(rule(nr, vec![ret(RET_ENOSYS)]));
        }
        program.push(ret(RET_ALLOW));
        program
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::Policy;
    use std::process::Command;

    pub(super) fn kernel_support() -> Result<(), String> {
        Err(if cfg!(windows) {
            "spawning needs Linux; on Windows, run wado under WSL2".to_string()
        } else {
            "wado confines a child only on Linux so far".to_string()
        })
    }

    pub(super) fn confine(_command: &mut Command, _policy: &Policy) -> std::io::Result<()> {
        panic!("confine needs a host that can: {:?}", kernel_support());
    }
}

/// The escape tests: each thing a confined child must not do, tried from inside
/// one. A test passes trivially on a host that cannot confine, unless
/// `WADO_REQUIRE_SANDBOX` says this host must.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{Policy, confine, kernel_support};
    use std::io;
    use std::net::{TcpListener, UdpSocket};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::process::{Command, Output};

    /// The variable that turns the test binary into a probe running one
    /// system call inside the sandbox.
    const PROBE: &str = "WADO_SANDBOX_PROBE";

    fn supported() -> bool {
        match kernel_support() {
            Ok(()) => true,
            Err(reason) => {
                assert!(
                    std::env::var_os("WADO_REQUIRE_SANDBOX").is_none(),
                    "this host must confine: {reason}"
                );
                false
            }
        }
    }

    fn policy(grants: &[&Path], network: bool) -> Policy {
        Policy {
            grants: grants.iter().map(|p| p.to_path_buf()).collect(),
            scratch: Vec::new(),
            network,
            tty: false,
        }
    }

    fn run(policy: &Policy, mut command: Command) -> Output {
        confine(&mut command, policy).expect("the rules build");
        command.output().expect("the child starts")
    }

    fn sh(policy: &Policy, script: &str) -> Output {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        run(policy, command)
    }

    /// Run probe `name` with `arg` in a child confined to `policy` plus the
    /// test binary's own directory, and return the `errno` it saw (0 for
    /// success).
    fn probe(policy: &Policy, name: &str, arg: &str) -> i32 {
        let exe = std::env::current_exe().expect("the test binary");
        let mut policy = policy.clone();
        policy
            .grants
            .push(exe.parent().expect("a directory").to_path_buf());
        let mut command = Command::new(&exe);
        command
            .args(["--exact", "sandbox::tests::probe_entry", "--test-threads=1"])
            .env(PROBE, format!("{name}={arg}"));
        let out = run(&policy, command);
        out.status
            .code()
            .unwrap_or_else(|| panic!("probe {name} died: {out:?}"))
    }

    fn errno(ok: bool) -> ! {
        let code = if ok {
            0
        } else {
            io::Error::last_os_error().raw_os_error().expect("an errno")
        };
        std::process::exit(code)
    }

    /// The probe's side: one system call, its `errno` as the exit code.
    #[test]
    fn probe_entry() {
        let Ok(spec) = std::env::var(PROBE) else {
            return;
        };
        let (name, arg) = spec.split_once('=').expect("name=arg");
        // SAFETY: each call takes plain integers or a buffer that outlives it.
        unsafe {
            match name {
                "tcp" => errno(std::net::TcpStream::connect(arg).is_ok()),
                "udp" => {
                    let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
                    errno(
                        fd >= 0
                            && UdpSocket::bind("127.0.0.1:0")
                                .is_ok_and(|s| s.send_to(b"x", arg).is_ok()),
                    )
                }
                "unix" => errno(std::os::unix::net::UnixStream::connect(arg).is_ok()),
                "pair-stream" | "pair-dgram" => {
                    let ty = if name == "pair-stream" {
                        libc::SOCK_STREAM
                    } else {
                        libc::SOCK_DGRAM
                    };
                    let mut fds = [0; 2];
                    errno(libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) == 0)
                }
                "setsid" => errno(libc::setsid() >= 0),
                "setpgid" => errno(libc::setpgid(0, 0) == 0),
                "tiocsti" => {
                    let c = b'x';
                    errno(libc::ioctl(0, libc::TIOCSTI, &raw const c) == 0)
                }
                "signal" => {
                    let pid: libc::pid_t = arg.parse().expect("a pid");
                    errno(libc::kill(pid, 0) == 0)
                }
                "io-uring" => {
                    let mut params = [0u8; 120];
                    errno(libc::syscall(libc::SYS_io_uring_setup, 1, params.as_mut_ptr()) >= 0)
                }
                other => panic!("no probe {other}"),
            }
        }
    }

    #[test]
    fn a_child_reaches_its_grants_and_nothing_else() {
        if !supported() {
            return;
        }
        let grant = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(grant.path().join("in"), "in").unwrap();
        std::fs::write(outside.path().join("out"), "out").unwrap();
        let policy = policy(&[grant.path()], false);
        let g = grant.path().display();
        let o = outside.path().display();

        assert!(
            sh(&policy, &format!("cat {g}/in && echo w > {g}/new"))
                .status
                .success()
        );
        for escape in [
            format!("cat {o}/out"),
            format!("echo w > {o}/new"),
            format!(
                "cat {g}/../{}/out",
                outside.path().file_name().unwrap().to_str().unwrap()
            ),
            format!("ln -s {o}/out {g}/link && cat {g}/link"),
            // The baseline is read-only, even to root.
            "touch /usr/wado-escape-test".to_string(),
        ] {
            let out = sh(&policy, &escape);
            assert!(!out.status.success(), "escaped with `{escape}`");
        }
        assert!(!Path::new("/usr/wado-escape-test").exists());
    }

    #[test]
    fn a_child_executes_inside_its_grants_only() {
        if !supported() {
            return;
        }
        let grant = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for dir in [grant.path(), outside.path()] {
            std::fs::copy("/bin/true", dir.join("true")).unwrap();
        }
        let policy = policy(&[grant.path()], false);
        let g = grant.path().display();
        let o = outside.path().display();

        assert!(sh(&policy, &format!("{g}/true")).status.success());
        assert!(!sh(&policy, &format!("{o}/true")).status.success());
        // A grandchild is held to the same rules.
        assert!(
            !sh(&policy, &format!("/bin/sh -c {o}/true"))
                .status
                .success()
        );
        let mut direct = Command::new(outside.path().join("true"));
        confine(&mut direct, &policy).unwrap();
        let denied = direct.status().unwrap_err();
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn without_the_network_no_socket_opens() {
        if !supported() {
            return;
        }
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp_addr = tcp.local_addr().unwrap().to_string();
        let udp_addr = udp.local_addr().unwrap().to_string();
        let closed = policy(&[], false);
        let open = policy(&[], true);

        assert_eq!(probe(&closed, "tcp", &tcp_addr), libc::EPERM);
        assert_eq!(probe(&closed, "udp", &udp_addr), libc::EPERM);
        assert_eq!(probe(&open, "tcp", &tcp_addr), 0);
        assert_eq!(probe(&open, "udp", &udp_addr), 0);
    }

    #[test]
    fn no_unix_socket_named_by_a_path_is_reachable() {
        if !supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let path = path.to_str().unwrap();
        // Not even one inside the grants, nor with the network.
        let policy = policy(&[dir.path()], true);

        assert_eq!(probe(&policy, "unix", path), libc::EPERM);
        assert_eq!(probe(&policy, "pair-dgram", ""), libc::EPERM);
        assert_eq!(probe(&policy, "pair-stream", ""), 0);
    }

    #[test]
    fn a_child_stays_in_its_group_and_away_from_other_processes() {
        if !supported() {
            return;
        }
        let policy = policy(&[], false);

        assert_eq!(probe(&policy, "setsid", ""), libc::EPERM);
        assert_eq!(probe(&policy, "setpgid", ""), libc::EPERM);
        assert_eq!(
            probe(&policy, "signal", &std::process::id().to_string()),
            libc::EPERM
        );
        assert_eq!(probe(&policy, "tiocsti", ""), libc::EPERM);
        assert_eq!(probe(&policy, "io-uring", ""), libc::ENOSYS);
    }
}
