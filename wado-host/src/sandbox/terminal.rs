//! The host's terminal, and the one child at a time that holds it.
//!
//! A child of an unconfined spawn shares its parent's process group, and with
//! it the parent's place on the terminal: it reads the terminal and sets its
//! mode. A confined child runs in a group of its own, which reads and sets the
//! terminal only as its foreground group. So the host treats a child whose
//! stdin is its terminal as a shell treats the job it runs: it hands the
//! terminal over, takes it back with its own mode when the child stops or ends,
//! and runs one such child in the foreground at a time.

use std::sync::{Condvar, Mutex};

use crate::sync::{lock, wait_while};

use super::Stdio;

/// Where a child's stdin goes, as the terminal sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Terminal {
    /// Not the host's terminal.
    Elsewhere,
    /// The host's terminal: the child reads it in the foreground or not at
    /// all, so the host follows each stop of the child.
    Inherited,
}

impl Terminal {
    pub(super) fn of(stdin: Stdio) -> Self {
        // SAFETY: plain system call.
        if stdin == Stdio::Inherit && unsafe { libc::isatty(STDIN) == 1 } {
            Terminal::Inherited
        } else {
            Terminal::Elsewhere
        }
    }
}

const STDIN: libc::c_int = 0;

/// Who holds the terminal among the host's children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holder {
    /// No child: the host, where it is in the foreground at all.
    Host,
    /// The child being spawned, whose pid is not known yet.
    Reserved,
    Child(libc::pid_t),
}

struct Hold {
    holder: Holder,
    /// The host's mode, restored when the terminal comes back from a child
    /// that left its own.
    host_mode: Option<libc::termios>,
}

static HOLD: Mutex<Hold> = Mutex::new(Hold {
    holder: Holder::Host,
    host_mode: None,
});
/// Notified each time the terminal comes back to the host.
static RETURNED: Condvar = Condvar::new();

/// The terminal set aside for a child being spawned. Dropped unheld, as when
/// the spawn fails, it comes back to the host.
pub(super) struct Reservation(());

/// Set the terminal aside for the child about to be spawned, where its stdin is
/// the terminal, the host holds it, and no other child does.
pub(super) fn reserve(terminal: Terminal) -> Option<Reservation> {
    let mut hold = lock(&HOLD);
    let free = terminal == Terminal::Inherited
        && hold.holder == Holder::Host
        && foreground() == own_group();
    if !free {
        return None;
    }
    hold.host_mode = mode();
    hold.holder = Holder::Reserved;
    Some(Reservation(()))
}

impl Reservation {
    /// The spawned child, whose group the backend made the foreground group.
    pub(super) fn held_by(self, pid: libc::pid_t) {
        lock(&HOLD).holder = Holder::Child(pid);
        std::mem::forget(self);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut hold = lock(&HOLD);
        if foreground() != own_group() {
            give_terminal(own_group());
        }
        hold.holder = Holder::Host;
        RETURNED.notify_all();
    }
}

/// The child `pid` stopped. Holding the terminal, it stopped as a job does at
/// Ctrl-Z, and the host stops with it, as it would sharing its group. Another
/// child holding the terminal, it read it out of turn, and waits until that
/// child is done. The host in the background, it read the terminal from
/// there, which stops the host too. Once the host holds the terminal again,
/// the child gets it back with the mode it left. The caller continues it.
pub(super) fn follow_stop(pid: libc::pid_t) {
    let mut hold = lock(&HOLD);
    let held = hold.holder == Holder::Child(pid);
    let child_mode = if held {
        let child_mode = mode();
        take_back(&mut hold, Mode::Host);
        child_mode
    } else {
        None
    };
    hold = wait_while(&RETURNED, hold, |hold| hold.holder != Holder::Host);
    if held || foreground() != own_group() {
        drop(hold);
        // SAFETY: plain system call; it returns once the group is continued.
        unsafe { libc::killpg(own_group(), libc::SIGTSTP) };
        hold = wait_while(&RETURNED, lock(&HOLD), |hold| hold.holder != Holder::Host);
    }
    if foreground() == own_group() {
        hold.host_mode = mode();
        if let Some(child_mode) = child_mode {
            set_mode(&child_mode);
        }
        give_terminal(pid);
        hold.holder = Holder::Child(pid);
    }
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

/// The terminal back with the host, in `mode`.
fn take_back(hold: &mut Hold, mode: Mode) {
    give_terminal(own_group());
    if mode == Mode::Host
        && let Some(host_mode) = hold.host_mode
    {
        set_mode(&host_mode);
    }
    hold.holder = Holder::Host;
    RETURNED.notify_all();
}

/// Make `group` the terminal's foreground group. A group in the background may
/// do so only with `SIGTTOU` blocked, as a shell does. A terminal that refuses,
/// as one that hung up does, stays as it was, and a child in the background
/// stops at its first read. Async-signal-safe, so a child may call it between
/// `fork` and `exec`.
pub(super) fn give_terminal(group: libc::pid_t) {
    // SAFETY: plain calls on signal sets that live on this stack.
    unsafe {
        let mut ttou: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut ttou);
        libc::sigaddset(&raw mut ttou, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const ttou, &raw mut old);
        libc::tcsetpgrp(STDIN, group);
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
}

/// The terminal's mode, where it answers.
fn mode() -> Option<libc::termios> {
    // SAFETY: `mode` is plain data the call fills.
    let mut mode: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: plain system call.
    (unsafe { libc::tcgetattr(STDIN, &raw mut mode) } == 0).then_some(mode)
}

/// Called by the terminal's foreground group, which `SIGTTOU` does not stop.
fn set_mode(mode: &libc::termios) {
    // SAFETY: plain system call.
    unsafe { libc::tcsetattr(STDIN, libc::TCSADRAIN, mode) };
}

fn foreground() -> libc::pid_t {
    // SAFETY: plain system call.
    unsafe { libc::tcgetpgrp(STDIN) }
}

fn own_group() -> libc::pid_t {
    // SAFETY: plain system call.
    unsafe { libc::getpgrp() }
}
