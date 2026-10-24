//! Linux confinement. Landlock restricts the filesystem, signals and abstract
//! UNIX sockets; seccomp covers what Landlock leaves: the network, UNIX sockets
//! named by a path, leaving the process group, typing into a terminal,
//! `ptrace`, and `io_uring`, whose operations seccomp never sees.
//!
//! Everything the child reads is built before the fork, so the window between
//! `fork` and `exec` makes only system calls.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command as StdCommand, Stdio as StdStdio};
use std::ptr;
use std::sync::OnceLock;

use super::{Access, Availability, Child, Command, Network, Policy, Spawned, Stdio};

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
    let filter = seccomp_filter(command.policy.network());
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
        std_command.pre_exec(move || confine_self(&ruleset, &filter, cwd.as_ref()));
    }
    let mut child = std_command.spawn()?;
    Ok(Spawned {
        child: Child::new(child.id() as libc::pid_t),
        stdin: child.stdin.take().map(OwnedFd::from),
        stdout: child.stdout.take().map(OwnedFd::from),
        stderr: child.stderr.take().map(OwnedFd::from),
    })
}

fn std_stdio(stdio: Stdio) -> StdStdio {
    match stdio {
        Stdio::Null => StdStdio::null(),
        Stdio::Inherit => StdStdio::inherit(),
        Stdio::Piped => StdStdio::piped(),
    }
}

/// Runs in the child between `fork` and `exec`.
fn confine_self(ruleset: &OwnedFd, filter: &[libc::sock_filter], cwd: Option<&File>) -> io::Result<()> {
    // A group of its own, so the host's kill reaches every descendant; the
    // filter below keeps them from leaving it.
    // SAFETY: each call below is a plain system call on memory the parent built.
    check(unsafe { libc::setpgid(0, 0) })?;
    if let Some(dir) = cwd {
        check(unsafe { libc::fchdir(dir.as_raw_fd()) })?;
    }
    check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })?;
    check_long(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) })?;
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).expect("the filter is built short"),
        filter: filter.as_ptr().cast_mut(),
    };
    check_long(unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0u32,
            &raw const program,
        )
    })?;
    // Confinement mediates what the child opens, never what it already holds.
    // Marked close-on-exec rather than closed, so the descriptor the standard
    // library reports a failed `exec` through survives until the `exec`.
    check_long(unsafe {
        libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC)
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
            Access::ReadExecute => ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR,
            Access::ReadWrite => ACCESS_FS_ALL,
        };
        if !file.metadata()?.is_dir() {
            allowed &= ACCESS_FS_FILE;
        }
        let beneath = PathBeneathAttr {
            allowed_access: allowed,
            parent_fd: file.as_raw_fd(),
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
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", rule.path.display())))?;
    }
    Ok(ruleset)
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

fn seccomp_filter(network: Network) -> Vec<libc::sock_filter> {
    let arch = AUDIT_ARCH.expect("availability refuses an architecture without a filter");
    let deny = libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32 & libc::SECCOMP_RET_DATA);
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
        asm.jump_if_eq(nr as u32, Label::Deny);
    }
    asm.jump_if_eq(libc::SYS_socket as u32, Label::Socket);
    asm.jump_if_eq(libc::SYS_ioctl as u32, Label::Ioctl);
    asm.ret(libc::SECCOMP_RET_ALLOW);

    asm.label(Label::Socket);
    match network {
        Network::Denied => asm.ret(deny),
        Network::Granted => {
            // A UNIX socket named by a path reaches a service outside the
            // sandbox; `socketpair` stays, reaching nothing outside the child.
            asm.load(seccomp_data_arg(0));
            asm.jump_if_eq(libc::AF_UNIX as u32, Label::Deny);
            asm.ret(libc::SECCOMP_RET_ALLOW);
        }
    }

    asm.label(Label::Ioctl);
    asm.load(seccomp_data_arg(1));
    asm.jump_if_eq(libc::TIOCSTI as u32, Label::Deny);
    asm.jump_if_eq(libc::TIOCLINUX as u32, Label::Deny);
    asm.ret(libc::SECCOMP_RET_ALLOW);

    asm.label(Label::Deny);
    asm.ret(deny);
    asm.label(Label::Kill);
    asm.ret(libc::SECCOMP_RET_KILL_PROCESS);
    asm.finish()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Label {
    ArchOk,
    Socket,
    Ioctl,
    Deny,
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
                Insn::Load(_) | Insn::JumpIfEq(..) | Insn::JumpIfGe(..) | Insn::Ret(_) => index += 1,
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
