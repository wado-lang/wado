//! Runtime proof for the BDP (BlobDataProvider) separation experiment.
//!
//! The casemap *feature* component bakes no Unicode data; it imports a `data`
//! interface that yields the postcard blob and loads it at runtime. We exercise
//! it two ways, proving both halves of the "shared data component" model:
//!
//!   1. feature component + host-supplied data import   (host fulfils `data`)
//!   2. composed component (feature + shared data)       (`data` satisfied
//!      internally by a sibling component; the host linker is empty)
//!
//! If fold/uppercase return correct Unicode results in both, the data-free
//! feature component works whether the blob comes from the host or from a
//! shared component composed in.

use anyhow::{Result, anyhow};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

wasmtime::component::bindgen!({
    world: "feature",
    path: "../casemap/wit",
});

// The feature component (imports `data`, exports `casemap`).
const FEATURE: &[u8] = include_bytes!("../../casemap-feature.wasm");
// feature + shared data composed into one import-free component.
const COMPOSED: &[u8] = include_bytes!("../../composed.wasm");
// The external blob, handed to the feature component by the host in scenario 1.
const BLOB: &[u8] = include_bytes!("../../casemap.blob");

struct Host {
    blob: Vec<u8>,
}

// Host implementation of the `data` import (used in scenario 1).
impl wado::icu_bdp::data::Host for Host {
    fn get_casemap_blob(&mut self) -> Vec<u8> {
        self.blob.clone()
    }
}

fn run(engine: &Engine, linker: &Linker<Host>, bytes: &[u8], scenario: &str) -> Result<bool> {
    let component = Component::new(engine, bytes)?;
    let mut store = Store::new(
        engine,
        Host {
            blob: BLOB.to_vec(),
        },
    );
    let feature = Feature::instantiate(&mut store, &component, linker)?;
    let cm = feature.wado_icu_bdp_casemap();

    let mut ok = true;
    let mut check = |label: &str, pass: bool| {
        if !pass {
            ok = false;
        }
        println!("  [{}] {label}", if pass { "OK" } else { "FAIL" });
    };

    println!("== {scenario} ==");

    let fold = cm
        .call_fold(&mut store, "Straße")?
        .map_err(|e| anyhow!("fold: {e}"))?;
    check(&format!("fold(\"Straße\") = {fold:?}"), fold == "strasse");

    let up_root = cm
        .call_uppercase(&mut store, "istanbul", "und")?
        .map_err(|e| anyhow!("uppercase und: {e}"))?;
    check(
        &format!("uppercase(\"istanbul\", und) = {up_root:?}"),
        up_root == "ISTANBUL",
    );

    let up_tr = cm
        .call_uppercase(&mut store, "istanbul", "tr")?
        .map_err(|e| anyhow!("uppercase tr: {e}"))?;
    check(
        &format!("uppercase(\"istanbul\", tr) = {up_tr:?}"),
        up_tr == "İSTANBUL",
    );

    let up_el = cm
        .call_uppercase(&mut store, "Γειά σου", "und")?
        .map_err(|e| anyhow!("uppercase el: {e}"))?;
    check(
        &format!("uppercase(\"Γειά σου\", und) = {up_el:?}"),
        up_el == "ΓΕΙΆ ΣΟΥ",
    );

    Ok(ok)
}

fn main() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    // Scenario 1: host fulfils the `data` import.
    let mut host_linker: Linker<Host> = Linker::new(&engine);
    wado::icu_bdp::data::add_to_linker::<_, wasmtime::component::HasSelf<_>>(
        &mut host_linker,
        |h| h,
    )?;
    let ok1 = run(
        &engine,
        &host_linker,
        FEATURE,
        "feature component + host-supplied data import",
    )?;

    // Scenario 2: composed component — `data` satisfied internally, empty linker.
    let empty_linker: Linker<Host> = Linker::new(&engine);
    let ok2 = run(
        &engine,
        &empty_linker,
        COMPOSED,
        "composed component (feature + shared data), empty host linker",
    )?;

    if !(ok1 && ok2) {
        anyhow::bail!("one or more checks failed");
    }
    println!("\nALL OK — data-free casemap works on a host blob AND on a composed shared-data component");
    Ok(())
}
