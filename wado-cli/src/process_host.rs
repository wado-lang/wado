//! The `wado` host for `core:process`: spawning a child confined to the
//! guest's grants plus a fixed baseline, through `wado_host::sandbox`.
//!
//! See WEP 2026-10-21 (`core:process`).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use wasmtime::component::{Access, HasData, Resource, ResourceTable};
use wasmtime_wasi::filesystem::Descriptor;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

#[cfg(unix)]
mod posix;
#[cfg(unix)]
pub use posix::ChildHandle;

/// No child runs on a host that cannot confine one.
#[cfg(not(unix))]
pub enum ChildHandle {}

wasmtime::component::bindgen!({
    inline: "package wado:process-host; world host { import core:process/process@0.1.0; }",
    path: "../wado-compiler/lib/core/process",
    world: "wado:process-host/host",
    require_store_data_send: true,
    imports: {
        "core:process/process.spawn": store | trappable,
        default: trappable,
    },
    with: {
        "wasi": wasmtime_wasi::p3::bindings,
        "core:process/types.child": ChildHandle,
    },
});

use self::core::process::process;
use self::core::process::types::{self, Availability, Command, ErrorCode, Spawned};

/// Whether a host offers spawning, settled once when it starts, and the grants
/// a child receives where it does.
#[derive(Clone)]
pub struct Spawning(Arc<Answer>);

enum Answer {
    /// The guest's directories in preopen order: the first is where a child
    /// starts unless the guest names another.
    Available(Vec<Grant>),
    Denied(String),
    Unsupported(String),
}

struct Grant {
    path: PathBuf,
    dir: Arc<File>,
}

/// Whether `--deny-process` turned spawning off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessGrant {
    Offered,
    Denied,
}

impl Spawning {
    /// Settle the answer for a guest granted `preopened_dirs`, as
    /// `(host_path, guest_path)` pairs.
    ///
    /// # Errors
    ///
    /// Returns an error if a granted directory cannot be opened.
    pub fn new(preopened_dirs: &[(String, String)], grant: ProcessGrant) -> Result<Self> {
        Ok(Self(Arc::new(answer(preopened_dirs, grant)?)))
    }
}

fn answer(preopened_dirs: &[(String, String)], grant: ProcessGrant) -> Result<Answer> {
    if grant == ProcessGrant::Denied {
        return Ok(Answer::Denied(
            "spawning is turned off by --deny-process".to_owned(),
        ));
    }
    if let Some(reason) = unsupported_reason() {
        return Ok(Answer::Unsupported(reason));
    }
    let mut grants = Vec::with_capacity(preopened_dirs.len());
    for (host, guest) in preopened_dirs {
        let path = std::path::absolute(host)?;
        // The child starts in the first grant, so `.` names it on both sides.
        if guest != "." && Path::new(guest) != path {
            return Ok(Answer::Denied(format!(
                "the guest directory `{guest}` is the host directory `{}`, so a path in a child's arguments would name another file",
                path.display()
            )));
        }
        let dir = Arc::new(File::open(&path)?);
        grants.push(Grant { path, dir });
    }
    Ok(Answer::Available(grants))
}

#[cfg(unix)]
fn unsupported_reason() -> Option<String> {
    match wado_host::sandbox::availability() {
        wado_host::sandbox::Availability::Available => None,
        wado_host::sandbox::Availability::Unsupported(reason) => Some(reason),
    }
}

#[cfg(not(unix))]
fn unsupported_reason() -> Option<String> {
    Some(format!(
        "{} cannot confine a child; run wado in WSL2",
        std::env::consts::OS
    ))
}

/// Where the guest's own standard streams go, which is where a child's
/// inherited ones go.
pub enum GuestStdio {
    /// The host's descriptors 0, 1 and 2.
    Host,
    /// `wado test`'s in-memory pipes. The guest reads no stdin there.
    Captured {
        stdout: MemoryOutputPipe,
        stderr: MemoryOutputPipe,
    },
}

/// A store's spawning state.
pub struct ProcessCtx {
    spawning: Spawning,
    stdio: GuestStdio,
}

impl ProcessCtx {
    #[must_use]
    pub fn new(spawning: Spawning, stdio: GuestStdio) -> Self {
        Self { spawning, stdio }
    }

    /// The grants a child receives. Traps where spawning is not offered: the
    /// interface requires the guest to ask `get-availability` first.
    fn grants(&self) -> wasmtime::Result<&[Grant]> {
        match &*self.spawning.0 {
            Answer::Available(grants) => Ok(grants),
            Answer::Denied(reason) | Answer::Unsupported(reason) => Err(wasmtime::format_err!(
                "core:process is unavailable: {reason}"
            )),
        }
    }
}

pub struct ProcessView<'a> {
    pub ctx: &'a mut ProcessCtx,
    pub table: &'a mut ResourceTable,
}

/// The [`HasData`] the bindings reach a store's [`ProcessView`] through.
pub struct ProcessHost;

impl HasData for ProcessHost {
    type Data<'a> = ProcessView<'a>;
}

impl types::Host for ProcessView<'_> {}

impl types::HostChild for ProcessView<'_> {
    fn kill(&mut self, child: Resource<ChildHandle>) -> wasmtime::Result<()> {
        let handle = self.table.get(&child)?;
        #[cfg(unix)]
        handle.kill();
        #[cfg(not(unix))]
        match *handle {}
        Ok(())
    }

    fn drop(&mut self, child: Resource<ChildHandle>) -> wasmtime::Result<()> {
        let handle = self.table.delete(child)?;
        #[cfg(unix)]
        handle.kill();
        #[cfg(not(unix))]
        match handle {}
        Ok(())
    }
}

impl process::Host for ProcessView<'_> {
    fn get_availability(&mut self) -> wasmtime::Result<Availability> {
        Ok(match &*self.ctx.spawning.0 {
            Answer::Available(_) => Availability::Available,
            Answer::Denied(_) => Availability::Denied,
            Answer::Unsupported(_) => Availability::Unsupported,
        })
    }
}

impl<U> process::HostWithStore<U> for ProcessHost {
    fn spawn(
        store: Access<'_, U, Self>,
        cmd: Command,
        cwd: Option<Resource<Descriptor>>,
    ) -> wasmtime::Result<Result<Spawned, ErrorCode>> {
        #[cfg(unix)]
        return posix::spawn(store, cmd, cwd);
        #[cfg(not(unix))]
        {
            let view = store.get();
            view.ctx.grants()?;
            unreachable!("a host that cannot confine a child answers `unsupported`")
        }
    }
}

/// Link `core:process` into `linker`, reaching each store's state through
/// `view`.
///
/// # Errors
///
/// Returns an error if the linker already defines the interfaces.
pub fn add_to_linker<T: Send + 'static>(
    linker: &mut wasmtime::component::Linker<T>,
    view: fn(&mut T) -> ProcessView<'_>,
) -> wasmtime::Result<()> {
    types::add_to_linker::<T, ProcessHost>(linker, view)?;
    process::add_to_linker::<T, ProcessHost>(linker, view)
}
