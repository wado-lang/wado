//! Runtime proof for a post-hoc slice of the bundled ICU component: the
//! casemap-only artifact re-linked from the whole asset, never rebuilt.

use anyhow::{Result, anyhow};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "icu-casemap",
    path: "../wit-casemap",
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

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: casemap-only <component.wasm>"))?;

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &path)?;

    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtxBuilder::new().inherit_stdio().build(),
            table: ResourceTable::new(),
        },
    );

    let icu = IcuCasemap::instantiate(&mut store, &component, &linker)?;
    let locale = icu.wado_icu_locale();
    let casemap = icu.wado_icu_casemap();

    let tr = locale
        .locale()
        .call_parse(&mut store, "tr-TR")?
        .map_err(|e| anyhow!("parse tr-TR: {e}"))?;
    let upper = casemap.call_uppercase(&mut store, "istanbul", tr)?;
    assert_eq!(upper, "İSTANBUL", "Turkish dotted capital I");
    println!("[OK] casemap.uppercase(\"istanbul\", tr) = {upper:?}");

    let fold = casemap.call_fold(&mut store, "Straße")?;
    assert_eq!(fold, "strasse", "sharp s folds to ss");
    println!("[OK] casemap.fold(\"Straße\") = {fold:?}");

    let en = locale
        .locale()
        .call_parse(&mut store, "en-US")?
        .map_err(|e| anyhow!("parse en-US: {e}"))?;
    let lower = casemap.call_lowercase(&mut store, "İSTANBUL", en)?;
    assert_eq!(lower, "i\u{307}stanbul", "en-US keeps the combining dot Turkish drops");
    println!("[OK] casemap.lowercase(\"İSTANBUL\", en) = {lower:?}");

    println!("\nALL OK — the re-linked slice's ICU4X data is correct");
    Ok(())
}
