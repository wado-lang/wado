//! The host's terminal, and the one child at a time that holds it.
//!
//! A child of an unconfined spawn shares its parent's process group, and with
//! it the parent's place on the terminal: it reads the terminal and sets its
//! mode. A confined child runs in a group of its own, which reads and sets the
//! terminal only as its foreground group. So the host treats a child that
//! inherits a stream from its terminal as a shell treats the job it runs: it
//! hands the terminal over, takes it back with its own mode when the child
//! stops or ends, and lets one such child hold it at a time.

use std::os::fd::RawFd;
use std::sync::{Condvar, Mutex, MutexGuard, Once};

use crate::sync::{lock, wait_while};

use super::{Command, Stdio};

/// Where a child's streams go, as the terminal sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Terminal {
    /// Not to the host's terminal.
    Elsewhere,
    /// At least one to the host's terminal, open on `fd`: the child uses it in
    /// the foreground or stops, so the host follows each stop of the child.
    Inherited(RawFd),
}

impl Terminal {
    pub(super) fn of(command: &Command<'_>) -> Self {
        [(0, command.stdin), (1, command.stdout), (2, command.stderr)]
            .into_iter()
            .filter(|&(_, stdio)| stdio == Stdio::Inherit)
            // SAFETY: plain system call.
            .find(|&(fd, _)| unsafe { libc::isatty(fd) == 1 })
            .map_or(Terminal::Elsewhere, |(fd, _)| Terminal::Inherited(fd))
    }
}

/// Who holds the terminal among the host's children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holder {
    /// No child: the host, where it is in the foreground at all.
    Host,
    /// The child being spawned, whose pid is not known yet.
    Reserved,
    Child(libc::pid_t),
    /// The child stopped holding it, and the host is stopping with it: the
    /// host has the terminal, but no other child may take it.
    Stopped(libc::pid_t),
}

struct Hold {
    holder: Holder,
    /// Where the host reaches the terminal a child holds.
    fd: RawFd,
    /// The host's mode, restored when the terminal comes back from a child
    /// that stopped or that a signal ended.
    host_mode: Option<libc::termios>,
}

static HOLD: Mutex<Hold> = Mutex::new(Hold {
    holder: Holder::Host,
    fd: 0,
    host_mode: None,
});
/// Notified each time the terminal comes back to the host.
static RETURNED: Condvar = Condvar::new();
static RESTORE_AT_EXIT: Once = Once::new();

/// The terminal set aside for a child being spawned. Dropped unheld, as when
/// the spawn fails, it comes back to the host.
pub(super) struct Reservation {
    fd: RawFd,
}

/// Set the terminal aside for the child about to be spawned, where it inherits
/// a stream from it and no other child holds it. A child that finds it held
/// starts in the background, and gets it in its turn.
pub(super) fn reserve(terminal: Terminal) -> Option<Reservation> {
    let Terminal::Inherited(fd) = terminal else {
        return None;
    };
    let mut hold = lock(&HOLD);
    if hold.holder != Holder::Host || foreground(fd) != own_group() {
        return None;
    }
    hand_over(&mut hold, fd, Holder::Reserved);
    Some(Reservation { fd })
}

impl Reservation {
    /// The descriptor of the terminal the backend hands to the child.
    pub(super) fn fd(&self) -> RawFd {
        self.fd
    }

    /// The spawned child, whose group the backend made the foreground group.
    pub(super) fn held_by(self, pid: libc::pid_t) {
        lock(&HOLD).holder = Holder::Child(pid);
        std::mem::forget(self);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        take_back(&mut lock(&HOLD), Mode::Child);
    }
}

/// What the host does with a child once [`follow_stop`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resume {
    Continue,
    /// Leave it stopped: continued, it would only stop again.
    Stay,
}

/// The child `pid`, which reaches the terminal on `fd`, stopped.
///
/// - Holding the terminal, it stopped as a job does at Ctrl-Z, and the host
///   stops with it, as it would sharing its group.
/// - Another child holding the terminal, it used it out of turn, and waits
///   until that child is done.
/// - The host in the background, it used the terminal from there, which stops
///   the host too.
///
/// Once the host is in the foreground again, the child gets the terminal, with
/// the mode it left.
///
/// The kernel discards a stop sent to an orphaned group, so there the host
/// follows what a child sharing its group would see: Ctrl-Z does nothing, and
/// the child stays stopped where a read from the background would fail.
pub(super) fn follow_stop(fd: RawFd, pid: libc::pid_t) -> Resume {
    let mut hold = lock(&HOLD);
    let held = hold.holder == Holder::Child(pid);
    let child_mode = if held {
        if orphaned() {
            return Resume::Continue;
        }
        let child_mode = mode(fd);
        give_back(&hold, Mode::Host);
        hold.holder = Holder::Stopped(pid);
        hold = stop_host(hold);
        child_mode
    } else {
        hold = wait_while(&RETURNED, hold, |hold| hold.holder != Holder::Host);
        if foreground(fd) != own_group() {
            if orphaned() {
                return Resume::Stay;
            }
            hold = stop_host(hold);
            hold = wait_while(&RETURNED, hold, |hold| hold.holder != Holder::Host);
        }
        None
    };
    if foreground(fd) == own_group() {
        hand_over(&mut hold, fd, Holder::Child(pid));
        if let Some(child_mode) = child_mode {
            set_mode(fd, &child_mode);
        }
        give_terminal(fd, pid);
    } else if held {
        take_back(&mut hold, Mode::Host);
    }
    Resume::Continue
}

/// Stop the host's group, as the child's stop would if they shared it, and
/// return once it is continued. Other threads run in the meantime, so the lock
/// is not held.
fn stop_host(hold: MutexGuard<'static, Hold>) -> MutexGuard<'static, Hold> {
    drop(hold);
    // SAFETY: plain system call.
    unsafe { libc::killpg(own_group(), libc::SIGTSTP) };
    lock(&HOLD)
}

/// The child `pid` ended. Holding the terminal, it gives it back, with the
/// host's mode where a signal ended it before it could restore that itself.
pub(super) fn release(pid: libc::pid_t, ended: End) {
    let mut hold = lock(&HOLD);
    if hold.holder != Holder::Child(pid) {
        return;
    }
    let mode = match ended {
        End::Exited => Mode::Child,
        End::Signaled => Mode::Host,
    };
    take_back(&mut hold, mode);
}

/// How a child ended, as far as its terminal is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum End {
    Exited,
    Signaled,
}

/// Which mode the terminal keeps when it comes back to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// What the child left: a program that ended on its own chose it.
    Child,
    /// The host's, as a shell restores its own when a job stops or is killed.
    Host,
}

/// Record that `holder` takes the terminal on `fd`, with the host's mode
/// saved to come back to. The caller makes its group the foreground group.
fn hand_over(hold: &mut Hold, fd: RawFd, holder: Holder) {
    hold.fd = fd;
    hold.host_mode = mode(fd);
    hold.holder = holder;
    RESTORE_AT_EXIT.call_once(|| {
        // SAFETY: registers a function that takes no arguments.
        unsafe { libc::atexit(restore_at_exit) };
    });
}

/// The terminal back with the host, in `mode`, and free for another child.
fn take_back(hold: &mut Hold, mode: Mode) {
    give_back(hold, mode);
    hold.holder = Holder::Host;
    RETURNED.notify_all();
}

fn give_back(hold: &Hold, mode: Mode) {
    give_terminal(hold.fd, own_group());
    if mode == Mode::Host
        && let Some(host_mode) = hold.host_mode
    {
        set_mode(hold.fd, &host_mode);
    }
}

/// A host that exits while a child holds its terminal gives it back first:
/// the shell that ran the host restores nothing for a job that exited.
extern "C" fn restore_at_exit() {
    // A thread holding the lock is mid-handover; waiting for it could deadlock
    // the exit.
    let Ok(hold) = HOLD.try_lock() else {
        return;
    };
    if hold.holder != Holder::Host {
        give_back(&hold, Mode::Host);
    }
}

/// Whether the host's group is orphaned, as far as the host alone shows it:
/// its parent is in the same group, or in another session.
fn orphaned() -> bool {
    // SAFETY: plain system calls.
    unsafe {
        let parent = libc::getppid();
        libc::getpgid(parent) == own_group() || libc::getsid(parent) != libc::getsid(0)
    }
}

/// Make `group` the foreground group of the terminal on `fd`. A group in the
/// background may do so only with `SIGTTOU` blocked, as a shell does. A
/// terminal that refuses, as one that hung up does, stays as it was, and a
/// child in the background stops at its first use of it. Async-signal-safe,
/// so a child may call it between `fork` and `exec`.
pub(super) fn give_terminal(fd: RawFd, group: libc::pid_t) {
    // SAFETY: plain calls on signal sets that live on this stack.
    unsafe {
        let mut ttou: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut ttou);
        libc::sigaddset(&raw mut ttou, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const ttou, &raw mut old);
        libc::tcsetpgrp(fd, group);
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
}

/// The terminal's mode, where it answers.
fn mode(fd: RawFd) -> Option<libc::termios> {
    // SAFETY: `mode` is plain data the call fills.
    let mut mode: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: plain system call.
    (unsafe { libc::tcgetattr(fd, &raw mut mode) } == 0).then_some(mode)
}

/// Set the terminal's mode, where the host's group is in the foreground: set
/// from the background, `SIGTTOU` would stop the host. A terminal that refused
/// to come back keeps the mode it has.
fn set_mode(fd: RawFd, mode: &libc::termios) {
    if foreground(fd) != own_group() {
        return;
    }
    // SAFETY: plain system call.
    unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, mode) };
}

fn foreground(fd: RawFd) -> libc::pid_t {
    // SAFETY: plain system call.
    unsafe { libc::tcgetpgrp(fd) }
}

fn own_group() -> libc::pid_t {
    // SAFETY: plain system call.
    unsafe { libc::getpgrp() }
}
