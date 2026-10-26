//! The `wado` host for `core:process`: spawning a child confined to the
//! guest's grants plus a fixed baseline, through `wado_host::sandbox`.
//!
//! See WEP 2026-10-21 (`core:process`).

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use wasmtime::component::{Access, HasData, Resource, ResourceTable};
use wasmtime_wasi::filesystem::Descriptor;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

#[cfg(unix)]
mod posix;
#[cfg(test)]
mod tests;
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
pub struct Spawning(Arc<Shared>);

struct Shared {
    answer: Answer,
    /// The programs a refusal was already reported for, so a guest that
    /// retries one warns once.
    refused: Mutex<HashSet<PathBuf>>,
}

enum Answer {
    Available(Grants),
    Denied(Unavailable),
    Unsupported(Unavailable),
}

/// Why spawning is not offered, and what would offer it where something can.
struct Unavailable {
    reason: String,
    fix: Option<String>,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "core:process is unavailable: {}", self.reason)?;
        if let Some(fix) = &self.fix {
            write!(f, "\nhelp: {fix}")?;
        }
        write!(
            f,
            "\nhelp: a guest that can do without spawning asks `availability()` first"
        )
    }
}

/// What a child may reach beyond the baseline.
struct Grants {
    /// The guest's directories in preopen order: the first is where a child
    /// starts unless the guest names another.
    dirs: Vec<Grant>,
    /// Read and execute only, from `--process-read`. The user config's
    /// `[process] read` joins them at the spawn.
    read: Vec<PathBuf>,
}

struct Grant {
    path: PathBuf,
    dir: Arc<File>,
}

/// Whether `--deny-process` turned spawning off, and the paths
/// `--process-read` opens to every child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessGrant {
    Offered { read: Vec<PathBuf> },
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
        Ok(Self(Arc::new(Shared {
            answer: answer(preopened_dirs, grant)?,
            refused: Mutex::default(),
        })))
    }
}

fn answer(preopened_dirs: &[(String, String)], grant: ProcessGrant) -> Result<Answer> {
    let ProcessGrant::Offered { read } = grant else {
        return Ok(Answer::Denied(Unavailable {
            reason: "spawning is turned off by --deny-process".to_owned(),
            fix: Some("run without `--deny-process`".to_owned()),
        }));
    };
    if let Some(reason) = unsupported_reason() {
        return Ok(Answer::Unsupported(Unavailable { reason, fix: None }));
    }
    let mut dirs = Vec::with_capacity(preopened_dirs.len());
    for (index, (host, guest)) in preopened_dirs.iter().enumerate() {
        let path = std::path::absolute(host)?;
        // The child starts in the first grant, so there `.` names it on both
        // sides; anywhere else `.` names another directory than the child's.
        if !(index == 0 && guest == ".") && Path::new(guest) != path {
            return Ok(Answer::Denied(Unavailable {
                reason: format!(
                    "the guest directory `{guest}` is the host directory `{}`, so a path in a child's arguments would name another file",
                    path.display()
                ),
                fix: Some(format!(
                    "grant it at its own path: `--dir {}`",
                    path.display()
                )),
            }));
        }
        let dir = Arc::new(File::open(&path)?);
        dirs.push(Grant { path, dir });
    }
    Ok(Answer::Available(Grants { dirs, read }))
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
    fn grants(&self) -> wasmtime::Result<&Grants> {
        match &self.spawning.0.answer {
            Answer::Available(grants) => Ok(grants),
            Answer::Denied(unavailable) | Answer::Unsupported(unavailable) => {
                Err(wasmtime::format_err!("{unavailable}"))
            }
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
        self.table.delete(child)?;
        Ok(())
    }
}

impl process::Host for ProcessView<'_> {
    fn get_availability(&mut self) -> wasmtime::Result<Availability> {
        Ok(match &self.ctx.spawning.0.answer {
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
