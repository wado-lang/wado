//! The `core:process` host where no child can be confined: `get-availability`
//! answers `unsupported`, so no child exists and a spawn traps.

use wasmtime::component::{Access, Resource};
use wasmtime_wasi::filesystem::Descriptor;

use super::{Command, ErrorCode, ProcessHost, Spawned};

pub enum ChildHandle {}

impl ChildHandle {
    pub(super) fn terminate(&self) {
        match *self {}
    }

    pub(super) fn kill(&self) {
        match *self {}
    }
}

pub(super) fn unsupported_reason() -> Option<String> {
    Some(format!(
        "{} cannot confine a child; run wado in WSL2",
        std::env::consts::OS
    ))
}

pub(super) fn spawn<U>(
    mut store: Access<'_, U, ProcessHost>,
    _cmd: Command,
    _cwd: Option<Resource<Descriptor>>,
) -> wasmtime::Result<Result<Spawned, ErrorCode>> {
    store.get().ctx.grants()?;
    unreachable!("a host that cannot confine a child answers `unsupported`")
}
