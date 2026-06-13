//! Runtime proof for the wado-bundled-icu component: instantiate it under
//! wasmtime and exercise the string-oriented interfaces (and properties) to
//! confirm the baked ICU4X data is real and works across the boundary.

use anyhow::{Result, anyhow};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    world: "icu",
    path: "../wit",
});

use exports::wado::icu::properties::GeneralCategory;

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
    let mut ok = true;
    let mut check = |label: &str, pass: bool| {
        if !pass {
            ok = false;
        }
        println!("[{}] {label}", if pass { "OK" } else { "FAIL" });
    };

    // --- casemap (locale-independent fold) ---
    let fold = icu.wado_icu_casemap().call_fold(&mut store, "Straße")?;
    check(&format!("casemap.fold(\"Straße\") = {fold:?}"), fold == "strasse");

    // --- casemap (locale-aware, via a locale resource handle) ---
    let loc = icu
        .wado_icu_locale()
        .locale()
        .call_parse(&mut store, "tr")?
        .map_err(|e| anyhow!("locale.parse: {e}"))?;
    let up = icu.wado_icu_casemap().call_uppercase(&mut store, "istanbul", loc)?;
    check(&format!("casemap.uppercase(\"istanbul\", tr) = {up:?}"), up == "İSTANBUL");

    // --- normalizer ---
    let nfc = icu.wado_icu_normalizer().call_nfc(&mut store, "e\u{0301}")?;
    check(&format!("normalizer.nfc(\"e+◌́\") = {nfc:?}"), nfc == "é");
    let nfd = icu.wado_icu_normalizer().call_nfd(&mut store, "é")?;
    check(&format!("normalizer.nfd(\"é\") = {nfd:?}"), nfd == "e\u{0301}");

    // --- segmenter (grapheme clusters) ---
    // "a" + family emoji (ZWJ sequence) + "b": the emoji is one grapheme.
    let text = "a👨‍👩‍👧b";
    let gr = icu.wado_icu_segmenter().call_graphemes(&mut store, text)?;
    check(
        &format!("segmenter.graphemes boundaries={gr:?} (3 clusters)"),
        gr.first() == Some(&0) && gr.last() == Some(&(text.len() as u32)) && gr.len() == 4,
    );

    // --- properties ---
    let cat = icu.wado_icu_properties().call_category(&mut store, 'A')?;
    check(
        &format!("properties.category('A') = {cat:?}"),
        matches!(cat, GeneralCategory::UppercaseLetter),
    );
    let script = icu.wado_icu_properties().call_script(&mut store, 'あ')?;
    check(&format!("properties.script('あ') = {script:?}"), script == "Hira");
    let emoji = icu.wado_icu_properties().call_emoji(&mut store, '😀')?;
    check("properties.emoji('😀') = true", emoji);
    let alpha = icu.wado_icu_properties().call_alphabetic(&mut store, 'A')?;
    check("properties.alphabetic('A') = true", alpha);

    if !ok {
        anyhow::bail!("one or more checks failed");
    }
    println!("\nALL OK — ICU4X data is live across the component boundary");
    Ok(())
}
