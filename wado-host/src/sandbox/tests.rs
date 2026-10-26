//! Escape tests: each one tries a way out of the sandbox and expects the
//! refusal, beside a control that shows the same act succeeds where granted.
//!
//! A system call no shell utility makes is tried by this test binary itself,
//! re-executed under the sandbox: [`probe`] performs the act its environment
//! names and exits with what came of it.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{
    Access, Availability, Command, ExitStatus, Network, Policy, Refusal, Refusals, Spawned, Stdio,
    Trace, availability, spawn,
};

const PROBE_VAR: &str = "WADO_SANDBOX_PROBE";
/// The exit code of a probe whose act the kernel refused with `EPERM` or
/// `EACCES`.
const DENIED: u8 = 13;

/// Set in a re-execution that runs under a filter holding a listener.
#[cfg(target_os = "linux")]
const ENCLOSED_VAR: &str = "WADO_SANDBOX_ENCLOSED";

/// A host under a sandbox that holds the one seccomp listener the kernel
/// allows still confines a child, and says it observes none of its refusals.
#[cfg(target_os = "linux")]
#[test]
fn under_an_enclosing_listener_a_child_runs_unobserved() {
    assert_available();
    // SAFETY: the closure makes only system calls on memory built before the
    // fork.
    let passed = unsafe {
        reexecute("sandbox::tests::enclosed", ENCLOSED_VAR, || {
            let allow = [libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ALLOW,
            }];
            let program = libc::sock_fprog {
                len: 1,
                filter: allow.as_ptr().cast_mut(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let listener = libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
                &raw const program,
            );
            // Kept open across the `exec`: the filter holds its listener only
            // while one is open.
            if listener < 0 || libc::fcntl(listener as libc::c_int, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    };
    assert!(passed);
}

/// Whether the test `test` passes in this binary re-executed with `var` set,
/// after `setup` runs between `fork` and `exec`.
///
/// # Safety
///
/// `setup` must be safe in the child of a multithreaded process: system calls
/// on memory built before the fork alone.
#[cfg(target_os = "linux")]
unsafe fn reexecute(
    test: &str,
    var: &str,
    setup: impl FnMut() -> std::io::Result<()> + Send + Sync + 'static,
) -> bool {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--test-threads=1", "--quiet"])
        .env(var, "1");
    // SAFETY: as the caller vouches.
    unsafe { command.pre_exec(setup) };
    command.status().unwrap().success()
}

/// Set in a re-execution that may not read a non-dumpable child's memory.
#[cfg(target_os = "linux")]
const BLIND_VAR: &str = "WADO_SANDBOX_BLIND";

/// A child that hides its memory from the host keeps the trace from reading a
/// path, which the trace says rather than leaving the call out.
#[cfg(target_os = "linux")]
#[test]
fn a_path_the_host_cannot_read_is_reported_unseen() {
    /// `CAP_SYS_PTRACE`, which reads a non-dumpable process's memory.
    const CAP_SYS_PTRACE: libc::c_ulong = 19;
    assert_available();
    // A root host drops the capability from what its `exec` grants it; any
    // other host never held it, and fails to drop it with `EPERM`.
    // SAFETY: the closure makes one system call.
    let passed = unsafe {
        reexecute("sandbox::tests::blind", BLIND_VAR, || {
            libc::prctl(libc::PR_CAPBSET_DROP, CAP_SYS_PTRACE, 0, 0, 0);
            Ok(())
        })
    };
    assert!(passed);
}

/// Not a test of its own: the half of the test above that runs blind.
#[cfg(target_os = "linux")]
#[test]
fn blind() {
    if std::env::var_os(BLIND_VAR).is_none() {
        return;
    }
    let tree = Tree::new();
    let (status, refusals) = probe_reported(
        tree.policy(Network::Denied),
        Trace::Files,
        "undumpable:/etc/passwd",
    );
    assert_eq!(status, OK);
    // Nothing was refused: the trace only could not see.
    assert_eq!(refusals.collect(), BTreeSet::new());
    assert!(refusals.unseen().is_some());
}

/// Not a test of its own: the half of the test above that runs enclosed.
#[cfg(target_os = "linux")]
#[test]
fn enclosed() {
    if std::env::var_os(ENCLOSED_VAR).is_none() {
        return;
    }
    let tree = Tree::new();
    let args = [OsString::from("-c"), OsString::from("exit 3")];
    let spawned = spawn(&Command {
        program: Path::new("/bin/sh"),
        args: &args,
        env: &[],
        cwd: None,
        stdin: Stdio::Null,
        stdout: Stdio::Null,
        stderr: Stdio::Null,
        policy: &tree.policy(Network::Denied),
        trace: Trace::Files,
    })
    .unwrap();
    assert_eq!(spawned.child.wait().unwrap(), ExitStatus::Exited(3));
    assert!(spawned.refusals.collect().is_empty());
    assert!(spawned.unobserved.is_some());
}

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
        "tmpfile" => {
            // SAFETY: a plain system call on a NUL-terminated path.
            unsafe {
                libc::open(
                    c_path(arg).as_ptr(),
                    libc::O_TMPFILE | libc::O_WRONLY,
                    0o600,
                )
            }
        }
        #[cfg(target_os = "linux")]
        "mknod" => {
            let (mode, path) = arg.split_once(':').unwrap();
            let mode = libc::mode_t::from_str_radix(mode, 8).unwrap();
            // SAFETY: a plain system call on a NUL-terminated path.
            unsafe { libc::mknod(c_path(path).as_ptr(), mode, 0) }
        }
        #[cfg(target_os = "linux")]
        "rename" => {
            let (from, to) = arg.split_once(':').unwrap();
            // SAFETY: a plain system call on NUL-terminated paths.
            unsafe { libc::rename(c_path(from).as_ptr(), c_path(to).as_ptr()) }
        }
        #[cfg(target_os = "linux")]
        "exec_nofollow" => execveat(libc::AT_FDCWD, arg, libc::AT_SYMLINK_NOFOLLOW),
        #[cfg(target_os = "linux")]
        "exec_fd" => {
            let (flags, path) = arg.split_once(':').unwrap();
            // SAFETY: a plain system call on a NUL-terminated path.
            let fd = unsafe { libc::open(c_path(path).as_ptr(), libc::O_PATH) };
            assert!(fd >= 0);
            execveat(fd, "", flags.parse().unwrap())
        }
        #[cfg(target_os = "linux")]
        "memfd" => {
            // SAFETY: plain system calls on NUL-terminated paths.
            unsafe {
                let fd = libc::memfd_create(c_path("x").as_ptr(), 0);
                assert!(fd >= 0);
                let link = c_path(&format!("/proc/self/fd/{fd}"));
                libc::open(link.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o600)
            }
        }
        #[cfg(target_os = "linux")]
        "undumpable" => {
            // SAFETY: plain system calls on a NUL-terminated path.
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                libc::open(c_path(arg).as_ptr(), libc::O_RDONLY)
            }
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

#[cfg(target_os = "linux")]
fn c_path(path: &str) -> std::ffi::CString {
    std::ffi::CString::new(path).unwrap()
}

/// `execveat(dir, path, [path], [], flags)`, returning only on failure.
#[cfg(target_os = "linux")]
fn execveat(dir: libc::c_int, path: &str, flags: libc::c_int) -> libc::c_int {
    let path = c_path(path);
    let argv = [path.as_ptr(), std::ptr::null()];
    let envp = [std::ptr::null::<libc::c_char>()];
    // SAFETY: `path`, `argv` and `envp` are NUL-terminated and outlive the call.
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            dir,
            path.as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            flags,
        ) as libc::c_int
    }
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
    run_refused(policy, Trace::Off, program, args, env).0
}

/// How the child ended, and what the sandbox refused it on the way.
fn run_refused(
    policy: &Policy,
    trace: Trace,
    program: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (ExitStatus, BTreeSet<Refusal>) {
    let (status, refusals) = run_reported(policy, trace, program, args, env);
    (status, refusals.collect())
}

fn run_reported(
    policy: &Policy,
    trace: Trace,
    program: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> (ExitStatus, Refusals) {
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
        trace,
    })
    .unwrap();
    let status = spawned.child.wait().unwrap();
    (status, spawned.refusals)
}

/// `sh -c script` under `policy`, watching its file calls.
#[cfg(target_os = "linux")]
fn sh_traced(policy: &Policy, script: &str) -> (ExitStatus, BTreeSet<Refusal>) {
    run_refused(
        policy,
        Trace::Files,
        Path::new("/bin/sh"),
        &["-c", script],
        &[("PATH", "/usr/bin:/bin")],
    )
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
fn probe_under(policy: Policy, act: &str) -> ExitStatus {
    probe_refused(policy, Trace::Off, act).0
}

fn probe_refused(policy: Policy, trace: Trace, act: &str) -> (ExitStatus, BTreeSet<Refusal>) {
    let (status, refusals) = probe_reported(policy, trace, act);
    (status, refusals.collect())
}

fn probe_reported(mut policy: Policy, trace: Trace, act: &str) -> (ExitStatus, Refusals) {
    let exe = std::env::current_exe().unwrap();
    policy.grant_path(exe.parent().unwrap(), Access::ReadExecute);
    run_reported(
        &policy,
        trace,
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

#[cfg(target_os = "linux")]
#[test]
fn the_host_learns_each_call_the_filter_refused() {
    assert_available();
    let tree = Tree::new();
    let socket = tree.outside().join("service.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let host = std::process::id();
    for (network, act, refusal) in [
        (Network::Denied, "tcp".to_owned(), Refusal::Network),
        (
            Network::Granted,
            format!("unix:{}", socket.display()),
            Refusal::UnixSocket,
        ),
        (Network::Denied, "setsid".to_owned(), Refusal::LeaveGroup),
        (Network::Denied, format!("ptrace:{host}"), Refusal::Trace),
        (Network::Denied, "io_uring".to_owned(), Refusal::IoUring),
        (
            Network::Denied,
            "tiocsti".to_owned(),
            Refusal::TerminalInput,
        ),
    ] {
        assert_eq!(
            probe_refused(tree.policy(network), Trace::Off, &act),
            (REFUSED, BTreeSet::from([refusal])),
            "{act}"
        );
    }
    assert_eq!(
        probe_refused(tree.policy(Network::Granted), Trace::Off, "tcp"),
        (OK, BTreeSet::new())
    );
}

/// A call the kernel fails before Landlock judges it is no refusal, however
/// its flags or its path spell that; beside each, a spelling Landlock judges.
#[cfg(target_os = "linux")]
#[test]
fn a_traced_call_is_judged_as_its_flags_and_spelling_say() {
    assert_available();
    let tree = Tree::new();
    let outside = tree.outside().canonicalize().unwrap();
    let script = write_script(&outside);
    fs::write(outside.join("file"), "").unwrap();
    symlink(&script, outside.join("link")).unwrap();
    let refused = |act: String| -> BTreeSet<Refusal> {
        probe_refused(tree.policy(Network::Denied), Trace::Files, &act)
            .1
            .into_iter()
            .filter(|refusal| matches!(refusal, Refusal::Path { path, .. } if path.starts_with(&outside)))
            .collect()
    };
    let paths = |paths: &[(PathBuf, Access)]| -> BTreeSet<Refusal> {
        paths
            .iter()
            .map(|(path, access)| refused_path(path, *access))
            .collect()
    };
    let o = outside.display();
    let write = |name: &str| (outside.join(name), Access::ReadWrite);
    // `O_TMPFILE` makes an unnamed file in the directory it opens.
    assert_eq!(
        refused(format!("tmpfile:{o}")),
        paths(&[(outside.clone(), Access::ReadWrite)])
    );
    assert_eq!(
        refused(format!("mknod:100644:{o}/node")),
        paths(&[write("node")])
    );
    assert_eq!(refused(format!("mknod:40755:{o}/node")), paths(&[]));
    assert_eq!(refused(format!("mknod:100644:{o}/node/")), paths(&[]));
    assert_eq!(
        refused(format!("rename:{o}/file:{o}/moved")),
        paths(&[write("file"), write("moved")])
    );
    assert_eq!(refused(format!("rename:{o}/file:{o}/moved/")), paths(&[]));
    let exec = paths(&[(script.clone(), Access::ReadExecute)]);
    let s = script.display();
    assert_eq!(refused(format!("exec_nofollow:{s}")), exec);
    assert_eq!(refused(format!("exec_nofollow:{o}/link")), paths(&[]));
    assert_eq!(
        refused(format!("exec_fd:{}:{s}", libc::AT_EMPTY_PATH)),
        exec
    );
    assert_eq!(refused(format!("exec_fd:0:{s}")), paths(&[]));
    // A descriptor's link names an object, not the path its text spells: a
    // memfd lies on no path a grant covers, and Landlock lets it through.
    let (_, refusals) = probe_refused(tree.policy(Network::Denied), Trace::Files, "memfd");
    assert!(
        !refusals.iter().any(
            |refusal| matches!(refusal, Refusal::Path { path, .. } if path.to_string_lossy().contains("memfd"))
        ),
        "{refusals:?}"
    );
}

/// A refused path, with the directory a grant must cover: the path itself
/// where it is one, else the directory holding it.
#[cfg(target_os = "linux")]
fn refused_path(path: &Path, access: Access) -> Refusal {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap()
    };
    Refusal::Path {
        path: path.to_owned(),
        dir: dir.to_owned(),
        access,
    }
}

/// `exec` opens what a script's `#!` names, and Landlock judges that too.
#[cfg(target_os = "linux")]
#[test]
fn a_traced_child_reports_an_interpreter_outside_its_grants() {
    assert_available();
    let tree = Tree::new();
    let outside = tree.outside().canonicalize().unwrap();
    let granted = tree.granted().canonicalize().unwrap();
    let interpreter = outside.join("interp");
    fs::copy("/bin/sh", &interpreter).unwrap();
    let tool = granted.join("tool");
    fs::write(&tool, format!("#!{}\nexit 0\n", interpreter.display())).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let policy = tree.policy(Network::Denied);
    assert_eq!(
        policy.resolved().refused_interpreter(&tool),
        Some(interpreter.clone())
    );
    assert_eq!(
        sh_traced(&policy, &tool.display().to_string()).1,
        BTreeSet::from([refused_path(&interpreter, Access::ReadExecute)])
    );
}
/// `sh` reads with a builtin, so the child itself opens each file: a
/// descendant's `/proc/self` is not the child's.
#[cfg(target_os = "linux")]
#[test]
fn reads_its_own_proc_entry_and_the_system_s_but_no_other_process_s() {
    assert_available();
    let tree = Tree::new();
    let policy = tree.policy(Network::Denied);
    let own = "read l < /proc/self/status && read l < /proc/cpuinfo && read l < /proc/mounts \
               && cd /proc && read l < self/status";
    assert_eq!(sh(&policy, own), OK);
    assert_eq!(sh_traced(&policy, own), (OK, BTreeSet::new()));
    let host = format!("read l < /proc/{}/status", std::process::id());
    assert_ne!(sh(&policy, &host), OK);
}

/// A path counts once it exists for the call: a read of a missing file fails
/// with `ENOENT` whatever the grants.
#[cfg(target_os = "linux")]
#[test]
fn a_traced_child_reports_each_path_outside_its_grants() {
    assert_available();
    let tree = Tree::new();
    let outside = tree.outside().canonicalize().unwrap();
    let granted = tree.granted().canonicalize().unwrap();
    fs::write(outside.join("out.txt"), "out").unwrap();
    fs::write(outside.join("rel.txt"), "rel").unwrap();
    let policy = tree.policy(Network::Denied);
    let script = format!(
        "cat {o}/out.txt; echo x > {o}/new.txt; cat {o}/missing.txt; \
         echo x > {g}/in.txt; cat {g}/in.txt; cd {o} && cat rel.txt",
        o = outside.display(),
        g = granted.display(),
    );
    let path = |name: &str, access| refused_path(&outside.join(name), access);
    assert_eq!(
        sh_traced(&policy, &script).1,
        BTreeSet::from([
            path("out.txt", Access::ReadExecute),
            path("new.txt", Access::ReadWrite),
            path("rel.txt", Access::ReadExecute),
        ])
    );
    assert_eq!(
        run_refused(
            &policy,
            Trace::Off,
            Path::new("/bin/sh"),
            &["-c", &script],
            &[]
        )
        .1,
        BTreeSet::new()
    );
}

/// A call that fails before Landlock judges it, or names no path Landlock
/// mediates, is no refusal, and a path resolves as the caller sees it.
#[cfg(target_os = "linux")]
#[test]
fn a_traced_child_reports_only_what_landlock_judges() {
    assert_available();
    let tree = Tree::new();
    let outside = tree.outside().canonicalize().unwrap();
    let granted = tree.granted().canonicalize().unwrap();
    fs::write(outside.join("out.txt"), "out").unwrap();
    fs::write(outside.join("linked.txt"), "linked").unwrap();
    fs::create_dir(outside.join("sub")).unwrap();
    let policy = tree.policy(Network::Denied);
    let script = format!(
        "{{ echo x > /dev/stdout; }} | cat; mkdir {o}; mkdir -p {o}; rm -f {o}/missing.txt; \
         touch /; rmdir /; rmdir {g}/.; rmdir {o}/sub/./; rmdir {o}/sub/../; \
         ln -s {o}/linked.txt {g}/link && rm {g}/link; \
         cat {o}/out.txt/; mv {o}/missing.txt {o}/moved.txt; rm {o}/out.txt; rmdir {g}",
        o = outside.display(),
        g = granted.display(),
    );
    // `mkdir` and `rm` run as grandchildren, refused their own `/proc` entry.
    let refused: BTreeSet<Refusal> = sh_traced(&policy, &script)
        .1
        .into_iter()
        .filter(|refusal| !matches!(refusal, Refusal::ProcessEntry { .. }))
        .collect();
    // Removing the grant itself takes a right on the directory above it.
    let removed = |path: PathBuf| Refusal::Path {
        dir: path.parent().unwrap().to_owned(),
        path,
        access: Access::ReadWrite,
    };
    assert_eq!(
        refused,
        BTreeSet::from([removed(outside.join("out.txt")), removed(granted)])
    );
}

/// A per-process entry of `/proc` is reported without its pid, which changes
/// with every run, so no grant names it.
#[cfg(target_os = "linux")]
#[test]
fn a_descendant_s_own_proc_entry_is_reported_without_its_pid() {
    assert_available();
    let tree = Tree::new();
    let policy = tree.policy(Network::Denied);
    assert_eq!(
        sh_traced(&policy, "sh -c 'read l < /proc/self/status'").1,
        BTreeSet::from([Refusal::ProcessEntry {
            access: Access::ReadExecute
        }])
    );
    // The child's own entry is granted for reading alone.
    assert_eq!(
        sh_traced(&policy, "echo 0 > /proc/self/oom_score_adj").1,
        BTreeSet::from([Refusal::ProcessEntry {
            access: Access::ReadWrite
        }])
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
        trace: Trace::Off,
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
        trace: Trace::Off,
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
        trace: Trace::Off,
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
        trace: Trace::Off,
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
        trace: Trace::Off,
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
fn the_resolved_policy_answers_what_the_sandbox_executes() {
    let tree = Tree::new();
    let policy = tree.policy(Network::Denied);
    let outside = write_script(&tree.outside());
    let linked = tree.granted().join("linked");
    symlink(&outside, &linked).unwrap();
    // The sandbox confines through the descriptor, wherever its path moved.
    let moved = tree.root.path().join("moved");
    fs::rename(tree.granted(), &moved).unwrap();
    let granted = write_script(&moved);
    let resolved = policy.resolved();
    let executes =
        |program: &Path| resolved.permits(&fs::canonicalize(program).unwrap(), Access::ReadExecute);
    assert!(executes(&granted));
    assert!(executes(Path::new("/bin/sh")));
    assert!(!executes(&outside));
    // The sandbox follows the link to what it names.
    assert!(!executes(&moved.join("linked")));
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
