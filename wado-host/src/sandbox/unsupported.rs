//! A host with no backend: it cannot confine a child, so it offers no spawning.

use std::fs::File;
use std::io;
use std::path::PathBuf;

use super::{Availability, Command, Spawned, Terminal};

pub(super) fn availability() -> Availability {
    Availability::Unsupported(format!(
        "{} cannot confine a child; under Windows, run wado in WSL2",
        std::env::consts::OS
    ))
}

pub(super) fn spawn(_command: &Command<'_>, _terminal: Terminal) -> io::Result<Spawned> {
    unreachable!("`sandbox::spawn` asserts availability first")
}

pub(super) fn descriptor_path(_file: &File) -> io::Result<PathBuf> {
    unreachable!("a granted directory is resolved only where a child can be confined")
}
