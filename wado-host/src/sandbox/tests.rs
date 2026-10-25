//! Escape tests: each one tries a way out of the sandbox and expects the
//! refusal, beside a control that shows the same act succeeds where granted.
//!
//! A system call no shell utility makes is tried by this test binary itself,
//! re-executed under the sandbox: [`probe`] performs the act its environment
//! names and exits with what came of it.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    Access, Availability, Command, ExitStatus, Network, Policy, Spawned, Stdio, availability, spawn,
};

const PROBE_VAR: &str = "WADO_SANDBOX_PROBE";
/// The exit code of a probe whose act the kernel refused with `EPERM` or
/// `EACCES`.
const DENIED: u8 = 13;

/// Not a test of its own: the act a sandboxed re-execution performs.
#[test]
fn probe() {
    let Ok(act) = std::env::var(PROBE_VAR) else {
        return;
    };
    let (name, arg) = act.split_once(':').unwrap_or((&act, ""));
    let ret = match name {
        "ok" => 0,
        // Binding, not creating, the socket: Seatbelt leaves `socket` alone.
        "tcp" => match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(_) => 0,
            Err(err) => exit_with(&err),
        },
        "udp" => match std::net::UdpSocket::bind("127.0.0.1:0") {
            Ok(_) => 0,
            Err(err) => exit_with(&err),
        },
        "unix" => match std::os::unix::net::UnixStream::connect(arg) {
            Ok(_) => 0,
            Err(err) => exit_with(&err),
        },
        #[cfg(target_os = "linux")]
        "setsid" => {
            // The child leads its group, where `setsid` fails unconfined too, so
            // the call is made by a descendant that does not.
            // SAFETY: the forked child makes two system calls and exits.
            match unsafe { libc::fork() } {
                0 => unsafe {
                    let code = if libc::setsid() < 0 {
                        *libc::__errno_location()
                    } else {
                        0
                    };
                    libc::_exit(code)
                },
                pid => {
                    let mut status = 0;
                    // SAFETY: plain system call on our own child.
                    unsafe { libc::waitpid(pid, &raw mut status, 0) };
                    match libc::WEXITSTATUS(status) {
                        0 => 0,
                        errno => exit_with(&std::io::Error::from_raw_os_error(errno)),
                    }
                }
            }
        }
        "tiocsti" => {
            let byte = b'x';
            // SAFETY: TIOCSTI reads one byte from `byte`.
            unsafe { libc::ioctl(0, libc::TIOCSTI, &raw const byte) }
        }
        "ptrace" => {
            let pid: libc::pid_t = arg.parse().unwrap();
            // SAFETY: plain system call.
            #[cfg(target_os = "linux")]
            let ret = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid, 0, 0) as libc::c_int };
            // SAFETY: plain system call.
            #[cfg(target_os = "macos")]
            let ret = unsafe { libc::ptrace(libc::PT_ATTACHEXC, pid, std::ptr::null_mut(), 0) };
            ret
        }
        "signal" => {
            let pid: libc::pid_t = arg.parse().unwrap();
            // SAFETY: signal 0 only checks the permission.
            unsafe { libc::kill(pid, 0) }
        }
        "fd" => {
            let fd: i32 = arg.parse().unwrap();
            let mut byte = 0u8;
            // SAFETY: reads one byte into `byte`.
            unsafe { libc::read(fd, (&raw mut byte).cast(), 1) as libc::c_int }
        }
        #[cfg(target_os = "linux")]
        "io_uring" => {
            // SAFETY: the kernel writes the params it reads.
            let mut params = [0u8; 120];
            unsafe {
                libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) as libc::c_int
            }
        }
        other => panic!("unknown probe `{other}`"),
    };
    if ret < 0 {
        exit_with(&std::io::Error::last_os_error());
    }
    std::process::exit(0);
}

fn exit_with(err: &std::io::Error) -> ! {
    if let Some(libc::EPERM | libc::EACCES) = err.raw_os_error() {
        std::process::exit(i32::from(DENIED));
    }
    eprintln!("probe: {err}");
    std::process::exit(2);
}

/// A scratch tree: `granted/` the child may reach, `outside/` it may not.
struct Tree {
    root: tempfile::TempDir,
}

impl Tree {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("granted")).unwrap();
        fs::create_dir(root.path().join("outside")).unwrap();
        Self { root }
    }

    fn granted(&self) -> PathBuf {
        self.root.path().join("granted")
    }

    fn outside(&self) -> PathBuf {
        self.root.path().join("outside")
    }

    fn policy(&self, network: Network) -> Policy {
        let mut policy = Policy::new(network);
        let dir = File::open(self.granted()).unwrap();
        policy.grant_dir(self.granted(), Arc::new(dir));
        policy
    }
}

fn assert_available() {
    assert_eq!(availability(), Availability::Available);
}

fn run(policy: &Policy, program: &Path, args: &[&str], env: &[(&str, &str)]) -> ExitStatus {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let env: Vec<(OsString, OsString)> = env
        .iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect();
    let spawned = spawn(&Command {
        program,
        args: &args,
        env: &env,
        cwd: None,
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Inherit,
        policy,
    })
    .unwrap();
    spawned.child.wait().unwrap()
}

fn sh(policy: &Policy, script: &str) -> ExitStatus {
    run(
        policy,
        Path::new("/bin/sh"),
        &["-c", script],
        &[("PATH", "/usr/bin:/bin")],
    )
}

/// This test binary under `policy`, with its own directory readable, performing
/// `act`.
fn probe_under(mut policy: Policy, act: &str) -> ExitStatus {
    let exe = std::env::current_exe().unwrap();
    policy.grant_path(exe.parent().unwrap(), Access::ReadExecute);
    run(
        &policy,
        &exe,
        &[
            "--exact",
            "sandbox::tests::probe",
            "--test-threads=1",
            "--quiet",
        ],
        &[(PROBE_VAR, act)],
    )
}

const OK: ExitStatus = ExitStatus::Exited(0);
const REFUSED: ExitStatus = ExitStatus::Exited(DENIED);

#[test]
fn the_probe_runs_confined() {
    assert_available();
    let tree = Tree::new();
    assert_eq!(probe_under(tree.policy(Network::Denied), "ok"), OK);
}

#[test]
fn reads_only_what_is_granted() {
    assert_available();
    let tree = Tree::new();
    fs::write(tree.granted().join("in.txt"), "in").unwrap();
    fs::write(tree.outside().join("out.txt"), "out").unwrap();
    let policy = tree.policy(Network::Denied);
    let read = |path: &Path| sh(&policy, &format!("cat '{}'", path.display()));
    assert_eq!(read(&tree.granted().join("in.txt")), OK);
    assert_ne!(read(&tree.outside().join("out.txt")), OK);
}

#[test]
fn reads_the_system_configuration() {
    assert_available();
    let tree = Tree::new();
    assert_eq!(sh(&tree.policy(Network::Denied), "cat /etc/passwd"), OK);
}

#[test]
fn writes_nothing_under_a_read_only_subtree() {
    assert_available();
    let tree = Tree::new();
    let mut policy = Policy::new(Network::Denied);
    policy.grant_path(tree.granted(), Access::ReadExecute);
    let target = tree.granted().join("new.txt");
    assert_ne!(sh(&policy, &format!("echo x > '{}'", target.display())), OK);
    assert!(!target.exists());
    let policy = tree.policy(Network::Denied);
    assert_eq!(sh(&policy, &format!("echo x > '{}'", target.display())), OK);
}

fn write_script(dir: &Path) -> PathBuf {
    let path = dir.join("script");
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn executes_nothing_outside_the_grants_even_through_a_grandchild() {
    assert_available();
    let tree = Tree::new();
    let inside = write_script(&tree.granted());
    let outside = write_script(&tree.outside());
    let policy = tree.policy(Network::Denied);
    assert_eq!(sh(&policy, &format!("'{}'", inside.display())), OK);
    assert_ne!(sh(&policy, &format!("'{}'", outside.display())), OK);
    assert_ne!(
        sh(&policy, &format!("/bin/sh -c \"'{}'\"", outside.display())),
        OK
    );
}

#[test]
fn the_network_is_all_or_none() {
    assert_available();
    let tree = Tree::new();
    assert_eq!(probe_under(tree.policy(Network::Denied), "tcp"), REFUSED);
    assert_eq!(probe_under(tree.policy(Network::Denied), "udp"), REFUSED);
    assert_eq!(probe_under(tree.policy(Network::Granted), "tcp"), OK);
    assert_eq!(probe_under(tree.policy(Network::Granted), "udp"), OK);
}

#[test]
fn no_unix_socket_by_path_even_with_the_network() {
    assert_available();
    let tree = Tree::new();
    let path = tree.outside().join("service.sock");
    let _listener = UnixListener::bind(&path).unwrap();
    let act = format!("unix:{}", path.display());
    assert_eq!(probe_under(tree.policy(Network::Granted), &act), REFUSED);
}

/// Seatbelt cannot stop `setsid`, a known gap of the WEP.
#[cfg(target_os = "linux")]
#[test]
fn a_descendant_cannot_leave_the_group() {
    assert_available();
    let tree = Tree::new();
    assert_eq!(probe_under(tree.policy(Network::Denied), "setsid"), REFUSED);
}

#[test]
fn no_input_is_pushed_into_a_terminal() {
    assert_available();
    let tree = Tree::new();
    // Stdin is `/dev/null`, so an unconfined TIOCSTI fails with ENOTTY; only
    // the filter answers EPERM.
    assert_eq!(
        probe_under(tree.policy(Network::Denied), "tiocsti"),
        REFUSED
    );
}

#[test]
fn reaches_no_process_outside_the_sandbox() {
    assert_available();
    let tree = Tree::new();
    let host = std::process::id().to_string();
    assert_eq!(
        probe_under(tree.policy(Network::Denied), &format!("signal:{host}")),
        REFUSED
    );
    assert_eq!(
        probe_under(tree.policy(Network::Denied), &format!("ptrace:{host}")),
        REFUSED
    );
}

#[cfg(target_os = "linux")]
#[test]
fn no_io_uring() {
    assert_available();
    let tree = Tree::new();
    assert_eq!(
        probe_under(tree.policy(Network::Denied), "io_uring"),
        REFUSED
    );
}

#[test]
fn no_escape_through_a_symbolic_link_or_dot_dot() {
    assert_available();
    let tree = Tree::new();
    let secret = tree.outside().join("secret.txt");
    fs::write(&secret, "secret").unwrap();
    let link = tree.granted().join("link");
    symlink(&secret, &link).unwrap();
    let policy = tree.policy(Network::Denied);
    assert_ne!(sh(&policy, &format!("cat '{}'", link.display())), OK);
    let dotdot = tree.granted().join("../outside/secret.txt");
    assert_ne!(sh(&policy, &format!("cat '{}'", dotdot.display())), OK);
}

#[test]
fn inherits_no_descriptor_the_host_held_open() {
    assert_available();
    let tree = Tree::new();
    let secret = tree.outside().join("secret.txt");
    fs::write(&secret, "secret").unwrap();
    // Opened without O_CLOEXEC, as a descriptor `wado` inherited from its own
    // parent would be.
    let path = std::ffi::CString::new(secret.to_str().unwrap()).unwrap();
    // SAFETY: plain system call on a NUL-terminated path.
    let raw = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    assert!(raw >= 0);
    // SAFETY: the descriptor was just opened and is owned here alone.
    let held = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut contents = String::new();
    File::from(held.try_clone().unwrap())
        .read_to_string(&mut contents)
        .unwrap();
    assert_eq!(contents, "secret");
    let status = probe_under(
        tree.policy(Network::Denied),
        &format!("fd:{}", held.as_raw_fd()),
    );
    assert_eq!(
        status,
        ExitStatus::Exited(2),
        "the descriptor must be closed in the child"
    );
}

#[test]
fn starts_in_the_directory_it_is_given() {
    assert_available();
    let tree = Tree::new();
    fs::write(tree.granted().join("here.txt"), "here").unwrap();
    let policy = tree.policy(Network::Denied);
    let dir = File::open(tree.granted()).unwrap();
    let args: Vec<OsString> = ["-c", "cat here.txt"].iter().map(OsString::from).collect();
    let spawned = spawn(&Command {
        program: Path::new("/bin/sh"),
        args: &args,
        env: &[],
        cwd: Some(&dir),
        stdin: Stdio::Null,
        stdout: Stdio::Piped,
        stderr: Stdio::Inherit,
        policy: &policy,
    })
    .unwrap();
    let mut out = String::new();
    File::from(spawned.stdout.unwrap())
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(spawned.child.wait().unwrap(), OK);
    assert_eq!(out, "here");
}

#[test]
fn a_group_kill_ends_every_descendant() {
    assert_available();
    let tree = Tree::new();
    let policy = tree.policy(Network::Denied);
    let args: Vec<OsString> = ["-c", "sleep 60 & sleep 60; wait"]
        .iter()
        .map(OsString::from)
        .collect();
    let spawned = spawn(&Command {
        program: Path::new("/bin/sh"),
        args: &args,
        env: &[(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
        cwd: None,
        stdin: Stdio::Null,
        stdout: Stdio::Piped,
        stderr: Stdio::Inherit,
        policy: &policy,
    })
    .unwrap();
    spawned.child.signal_group(libc::SIGKILL).unwrap();
    assert_eq!(
        spawned.child.wait().unwrap(),
        ExitStatus::Signaled(libc::SIGKILL as u8)
    );
    // Every descendant held the pipe's write end; EOF means each one is gone.
    let mut rest = Vec::new();
    File::from(spawned.stdout.unwrap())
        .read_to_end(&mut rest)
        .unwrap();
    // A signal to a reaped group is a no-op rather than a stray kill.
    spawned.child.signal_group(libc::SIGKILL).unwrap();
}

#[test]
fn no_descendant_outlives_the_child() {
    assert_available();
    let tree = Tree::new();
    let policy = tree.policy(Network::Denied);
    let args: Vec<OsString> = ["-c", "sleep 60 &"].iter().map(OsString::from).collect();
    let spawned = spawn(&Command {
        program: Path::new("/bin/sh"),
        args: &args,
        env: &[(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
        cwd: None,
        stdin: Stdio::Null,
        stdout: Stdio::Piped,
        stderr: Stdio::Inherit,
        policy: &policy,
    })
    .unwrap();
    assert_eq!(spawned.child.wait().unwrap(), OK);
    // The background `sleep` held the pipe's write end; EOF means it is gone.
    let mut rest = Vec::new();
    File::from(spawned.stdout.unwrap())
        .read_to_end(&mut rest)
        .unwrap();
}

#[test]
fn a_missing_program_reports_not_found() {
    assert_available();
    let policy = Policy::new(Network::Denied);
    let result = spawn(&Command {
        program: Path::new("/nonexistent/program"),
        args: &[],
        env: &[],
        cwd: None,
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Null,
        policy: &policy,
    });
    assert_eq!(result.err().unwrap().kind(), std::io::ErrorKind::NotFound);
}

fn spawn_outside_the_grants(tree: &Tree) -> std::io::Result<Spawned> {
    assert_available();
    let outside = write_script(&tree.outside());
    let policy = tree.policy(Network::Denied);
    spawn(&Command {
        program: &outside,
        args: &[],
        env: &[],
        cwd: None,
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Null,
        policy: &policy,
    })
}

#[cfg(target_os = "linux")]
#[test]
fn a_program_outside_the_grants_reports_permission_denied() {
    assert_eq!(
        spawn_outside_the_grants(&Tree::new()).err().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

/// `sandbox-exec` starts and then fails to execute the program, a known gap
/// of the WEP.
#[cfg(target_os = "macos")]
#[test]
fn a_program_outside_the_grants_never_runs() {
    let tree = Tree::new();
    let spawned = spawn_outside_the_grants(&tree).unwrap();
    assert_ne!(spawned.child.wait().unwrap(), OK);
}

#[test]
fn a_grant_confines_to_exactly_its_path_whatever_it_holds() {
    assert_available();
    let root = tempfile::tempdir().unwrap();
    let granted = root.path().join(r#"a"b\c)d"#);
    let sibling = root.path().join(r#"a"b\c)d-sibling"#);
    for dir in [&granted, &sibling] {
        fs::create_dir(dir).unwrap();
        fs::write(dir.join("f.txt"), "f").unwrap();
    }
    let mut policy = Policy::new(Network::Denied);
    policy.grant_dir(granted.clone(), Arc::new(File::open(&granted).unwrap()));
    // The names hold no `'`, so single quotes pass them to `cat` as written.
    let read = |dir: &Path| sh(&policy, &format!("cat '{}'", dir.join("f.txt").display()));
    assert_eq!(read(&granted), OK);
    assert_ne!(read(&sibling), OK);
}
