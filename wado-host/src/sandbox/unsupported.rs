//! A host with no backend: it cannot confine a child, so it offers no spawning.

use std::io;

use super::{Availability, Command, Spawned};

pub(super) fn availability() -> Availability {
    Availability::Unsupported(format!(
        "{} cannot confine a child; under Windows, run wado in WSL2",
        std::env::consts::OS
    ))
}

pub(super) fn spawn(_command: &Command<'_>) -> io::Result<Spawned> {
    unreachable!("`sandbox::spawn` asserts availability first")
}
