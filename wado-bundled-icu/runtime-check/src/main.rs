//! Runtime proof for the wado-bundled-icu component: instantiate it under
//! wasmtime, satisfy its wasi imports, and call `uppercase-in` to confirm the
//! baked ICU4X case-mapping data is real and works across the boundary.

use anyhow::{Result, anyhow};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "icu",
    path: "../wit",
});

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

// The no_std wasm32-unknown-unknown core module wrapped by `wasm-tools
// component new`. It imports nothing, so the wasi linker below is unused but
// harmless.
const COMPONENT: &[u8] = include_bytes!("../../target/wado_bundled_icu.component.wasm");

fn main() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let component = Component::new(&engine, COMPONENT)?;
    let mut linker: Linker<Host> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtxBuilder::new().inherit_stderr().build(),
            table: ResourceTable::new(),
        },
    );

    let icu = Icu::instantiate(&mut store, &component, &linker)?;
    let casemap = icu.wado_icu_casemap();

    // (input, BCP-47 tag, expected) — the Turkish dotted-I is the classic proof
    // that locale-aware data (not ASCII toupper) is actually being used.
    let cases = [
        ("istanbul", "tr", "İSTANBUL"),
        ("istanbul", "en", "ISTANBUL"),
        ("straße", "de", "STRASSE"),
        ("hello", "und", "HELLO"),
    ];

    let mut all_ok = true;
    for (text, tag, want) in cases {
        let got = casemap
            .call_uppercase_in(&mut store, text, tag)?
            .map_err(|e| anyhow!("guest returned error: {e}"))?;
        let ok = got == want;
        all_ok &= ok;
        println!(
            "[{}] uppercase_in({text:?}, {tag:?}) = {got:?} (want {want:?})",
            if ok { "OK" } else { "FAIL" }
        );
    }

    if !all_ok {
        anyhow::bail!("one or more case-mapping results did not match");
    }
    println!("ALL OK — ICU4X data is live across the component boundary");
    Ok(())
}
