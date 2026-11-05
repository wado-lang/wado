//! The wasmtime host: WASI P3 plus `core:pjrt`, served by a PJRT plugin.

use std::sync::Arc;

use wado_host::guest::GuestState;
use wado_host::guest_views;
use wado_host::options::CliExit;
use wado_host::runner::{self, Program, RunOptions};
use wasmtime::component::{HasData, Resource};

pub use crate::plugin::{Buffer, Executable};
use crate::plugin::{Client, Plugin};

wasmtime::component::bindgen!({
    inline: "package wado:pjrt-host; world host { import core:pjrt/pjrt-host@0.1.0; }",
    path: "../wado-compiler/lib/core/pjrt",
    world: "wado:pjrt-host/host",
    require_store_data_send: true,
    imports: { default: trappable },
    with: {
        "core:pjrt/pjrt-host.buffer": Buffer,
        "core:pjrt/pjrt-host.executable": Executable,
    },
});

use self::core::pjrt::pjrt_host::{self, ElementType, HostBuffer, HostExecutable};
pub use self::core::pjrt::pjrt_host::{Analysis, Cost};

struct Host {
    guest: GuestState,
    // After `guest`, whose table's buffers and executables must go before their
    // client. Opened at first use: `libtpu.so` with no TPU has no client, and
    // still compiles for a topology.
    client: Option<Client>,
    plugin: Arc<Plugin>,
}

guest_views!(Host, guest);

impl Host {
    fn client(&mut self) -> Result<&Client, String> {
        if self.client.is_none() {
            self.client = Some(Client::open(Arc::clone(&self.plugin))?);
        }
        Ok(self.client.as_ref().expect("opened above"))
    }

    /// `made`, in the guest's hands.
    fn held<T: Send + 'static>(
        &mut self,
        made: Result<T, String>,
    ) -> wasmtime::Result<Result<Resource<T>, String>> {
        Ok(match made {
            Ok(value) => Ok(self.guest.table.push(value)?),
            Err(error) => Err(error),
        })
    }
}

/// `PJRT_Buffer_Type`, and the bytes one element takes.
fn pjrt_type(element: ElementType) -> (i32, usize) {
    match element {
        ElementType::Pred => (1, 1),
        ElementType::Int8 => (2, 1),
        ElementType::Int16 => (3, 2),
        ElementType::Int32 => (4, 4),
        ElementType::Int64 => (5, 8),
        ElementType::Uint8 => (6, 1),
        ElementType::Uint16 => (7, 2),
        ElementType::Uint32 => (8, 4),
        ElementType::Uint64 => (9, 8),
        ElementType::Float16 => (10, 2),
        ElementType::Float32 => (11, 4),
        ElementType::Float64 => (12, 8),
        ElementType::Bfloat16 => (13, 2),
    }
}

impl pjrt_host::Host for Host {
    fn platform(&mut self) -> wasmtime::Result<Result<String, String>> {
        Ok(self.client().and_then(Client::platform))
    }

    fn compile(
        &mut self,
        module: String,
    ) -> wasmtime::Result<Result<Resource<Executable>, String>> {
        let executable = self.client().and_then(|client| client.compile(&module));
        self.held(executable)
    }

    fn analyzed(
        &mut self,
        module: String,
        topology: String,
    ) -> wasmtime::Result<Result<Analysis, String>> {
        Ok(self.plugin.analyzed(&module, &topology))
    }

    fn upload(
        &mut self,
        element: ElementType,
        extents: Vec<i64>,
        elements: Vec<u8>,
    ) -> wasmtime::Result<Result<Resource<Buffer>, String>> {
        let (element_type, element_bytes) = pjrt_type(element);
        let buffer = self
            .client()
            .and_then(|client| client.upload(element_type, element_bytes, &extents, &elements));
        self.held(buffer)
    }
}

impl HostBuffer for Host {
    fn to_host(&mut self, buffer: Resource<Buffer>) -> wasmtime::Result<Result<Vec<u8>, String>> {
        Ok(self.guest.table.get(&buffer)?.to_host())
    }

    fn drop(&mut self, buffer: Resource<Buffer>) -> wasmtime::Result<()> {
        self.guest.table.delete(buffer)?;
        Ok(())
    }
}

impl HostExecutable for Host {
    fn execute(
        &mut self,
        executable: Resource<Executable>,
        arguments: Vec<Resource<Buffer>>,
        kept: Vec<i64>,
    ) -> wasmtime::Result<Result<Vec<Resource<Buffer>>, String>> {
        let held = arguments
            .iter()
            .map(|a| self.guest.table.get(a))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(
            match self.guest.table.get(&executable)?.execute(&held, &kept) {
                Ok(results) => Ok(results
                    .into_iter()
                    .map(|r| self.guest.table.push(r))
                    .collect::<Result<_, _>>()?),
                Err(error) => Err(error),
            },
        )
    }

    fn drop(&mut self, executable: Resource<Executable>) -> wasmtime::Result<()> {
        self.guest.table.delete(executable)?;
        Ok(())
    }
}

struct Pjrt;

impl HasData for Pjrt {
    type Data<'a> = &'a mut Host;
}

/// Runs a component that exports `wasi:cli/run` on `plugin`.
pub async fn run(
    program: &Program,
    options: &RunOptions,
    plugin: Arc<Plugin>,
) -> Result<(), CliExit> {
    let host = Host {
        guest: options.guest(program)?,
        client: None,
        plugin,
    };
    runner::run(
        "wado-run-pjrt",
        &program.wasm,
        &options.engine(),
        host,
        |_, _, linker| {
            pjrt_host::add_to_linker::<Host, Pjrt>(linker, |host| host)?;
            Ok(())
        },
    )
    .await
}
