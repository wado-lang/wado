//! The wasmtime host: WASI P3 plus `core:pjrt`, served by a PJRT plugin.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use wado_host::runner::{self, RunOptions, wasi_ctx};
use wasmtime::component::{HasData, Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

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
    ctx: WasiCtx,
    table: ResourceTable,
    // After `table`, whose buffers and executables must go before their client.
    // Opened at first use: `libtpu.so` with no TPU has no client, and still
    // compiles for a topology.
    client: Option<Client>,
    plugin: Arc<Plugin>,
}

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
            Ok(value) => Ok(self.table.push(value)?),
            Err(error) => Err(error),
        })
    }
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

/// `PJRT_Buffer_Type`.
fn pjrt_type(element: ElementType) -> i32 {
    match element {
        ElementType::Pred => 1,
        ElementType::Int8 => 2,
        ElementType::Int16 => 3,
        ElementType::Int32 => 4,
        ElementType::Int64 => 5,
        ElementType::Uint8 => 6,
        ElementType::Uint16 => 7,
        ElementType::Uint32 => 8,
        ElementType::Uint64 => 9,
        ElementType::Float16 => 10,
        ElementType::Float32 => 11,
        ElementType::Float64 => 12,
        ElementType::Bfloat16 => 13,
    }
}

impl pjrt_host::Host for Host {
    fn platform(&mut self) -> wasmtime::Result<String> {
        self.client()
            .and_then(Client::platform)
            .map_err(wasmtime::Error::msg)
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
        let buffer = self
            .client()
            .and_then(|client| client.upload(pjrt_type(element), &extents, &elements));
        self.held(buffer)
    }
}

impl HostBuffer for Host {
    fn to_host(&mut self, buffer: Resource<Buffer>) -> wasmtime::Result<Result<Vec<u8>, String>> {
        Ok(self.table.get(&buffer)?.to_host())
    }

    fn drop(&mut self, buffer: Resource<Buffer>) -> wasmtime::Result<()> {
        self.table.delete(buffer)?;
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
            .map(|a| self.table.get(a))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(match self.table.get(&executable)?.execute(&held, &kept) {
            Ok(results) => Ok(results
                .into_iter()
                .map(|r| self.table.push(r))
                .collect::<Result<_, _>>()?),
            Err(error) => Err(error),
        })
    }

    fn drop(&mut self, executable: Resource<Executable>) -> wasmtime::Result<()> {
        self.table.delete(executable)?;
        Ok(())
    }
}

struct Pjrt;

impl HasData for Pjrt {
    type Data<'a> = &'a mut Host;
}

/// Runs a component that exports `wasi:cli/run` on `plugin`.
pub async fn run(component: &Path, options: &RunOptions, plugin: Arc<Plugin>) -> Result<()> {
    let host = Host {
        ctx: wasi_ctx(options)?,
        table: ResourceTable::new(),
        client: None,
        plugin,
    };
    runner::run("wado-run-pjrt", component, options, host, |_, _, linker| {
        pjrt_host::add_to_linker::<Host, Pjrt>(linker, |host| host)?;
        Ok(())
    })
    .await
}
