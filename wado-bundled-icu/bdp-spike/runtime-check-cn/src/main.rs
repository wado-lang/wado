//! Runtime proof for the collator+normalizer marker-dedup demo.
//!
//! Both feature components are data-free and load the SAME shared blob (collator
//! + normalizer markers). We prove:
//!   - the normalizer feature works off the shared blob (NFC/NFD),
//!   - the collator feature works off the shared blob, including canonical
//!     equivalence ("e"+combining-acute compares EQUAL to precomposed "é"),
//!     which only succeeds if the collator is reading the normalization markers
//!     that the blob shares with the normalizer.
//! Each is exercised twice: with a host-supplied blob, and via a component
//! composed with the shared data component (empty host linker).

use anyhow::{Result, anyhow};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store};

mod coll {
    wasmtime::component::bindgen!({ world: "collator-feature", path: "../cn-wit" });
}
mod norm {
    wasmtime::component::bindgen!({ world: "normalizer-feature", path: "../cn-wit" });
}

use coll::exports::wado::icu_cn::collator::Ordering;

const COLLATOR_FEATURE: &[u8] = include_bytes!("../../collator-feature.wasm");
const COLLATOR_COMPOSED: &[u8] = include_bytes!("../../collator-composed.wasm");
const NORMALIZER_FEATURE: &[u8] = include_bytes!("../../normalizer-feature.wasm");
const NORMALIZER_COMPOSED: &[u8] = include_bytes!("../../normalizer-composed.wasm");
const SHARED_BLOB: &[u8] = include_bytes!("../../shared.blob");

struct Host {
    blob: Vec<u8>,
}
impl coll::wado::icu_cn::data::Host for Host {
    fn get_blob(&mut self) -> Vec<u8> {
        self.blob.clone()
    }
}
impl norm::wado::icu_cn::data::Host for Host {
    fn get_blob(&mut self) -> Vec<u8> {
        self.blob.clone()
    }
}

fn store(engine: &Engine) -> Store<Host> {
    Store::new(
        engine,
        Host {
            blob: SHARED_BLOB.to_vec(),
        },
    )
}

fn run_collator(engine: &Engine, bytes: &[u8], linker: &Linker<Host>, scenario: &str) -> Result<bool> {
    let component = Component::new(engine, bytes)?;
    let mut st = store(engine);
    let inst = coll::CollatorFeature::instantiate(&mut st, &component, linker)?;
    let c = inst.wado_icu_cn_collator();
    let mut ok = true;
    let mut check = |l: &str, p: bool| {
        ok &= p;
        println!("  [{}] {l}", if p { "OK" } else { "FAIL" });
    };
    println!("== collator: {scenario} ==");

    let r = c
        .call_compare(&mut st, "apple", "banana", "und")?
        .map_err(|e| anyhow!("compare: {e}"))?;
    check(&format!("compare(apple, banana) = {r:?}"), matches!(r, Ordering::Less));

    // Canonical equivalence: decomposed "é" vs precomposed "é" must be EQUAL.
    // This only works if the collator reads the shared normalization data.
    let r = c
        .call_compare(&mut st, "e\u{0301}", "\u{e9}", "und")?
        .map_err(|e| anyhow!("compare: {e}"))?;
    check(
        &format!("compare(e+◌́, é) = {r:?}  (canonical equivalence via shared NFD)"),
        matches!(r, Ordering::Equal),
    );
    Ok(ok)
}

fn run_normalizer(engine: &Engine, bytes: &[u8], linker: &Linker<Host>, scenario: &str) -> Result<bool> {
    let component = Component::new(engine, bytes)?;
    let mut st = store(engine);
    let inst = norm::NormalizerFeature::instantiate(&mut st, &component, linker)?;
    let n = inst.wado_icu_cn_normalizer();
    let mut ok = true;
    let mut check = |l: &str, p: bool| {
        ok &= p;
        println!("  [{}] {l}", if p { "OK" } else { "FAIL" });
    };
    println!("== normalizer: {scenario} ==");

    let nfc = n.call_nfc(&mut st, "e\u{0301}")?.map_err(|e| anyhow!("nfc: {e}"))?;
    check(&format!("nfc(e+◌́) = {nfc:?}"), nfc == "\u{e9}");
    let nfd = n.call_nfd(&mut st, "\u{e9}")?.map_err(|e| anyhow!("nfd: {e}"))?;
    check(&format!("nfd(é) = {nfd:?}"), nfd == "e\u{0301}");
    Ok(ok)
}

fn main() -> Result<()> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;

    let mut coll_host: Linker<Host> = Linker::new(&engine);
    coll::wado::icu_cn::data::add_to_linker::<_, HasSelf<_>>(&mut coll_host, |h| h)?;
    let mut norm_host: Linker<Host> = Linker::new(&engine);
    norm::wado::icu_cn::data::add_to_linker::<_, HasSelf<_>>(&mut norm_host, |h| h)?;
    let empty: Linker<Host> = Linker::new(&engine);

    let mut ok = true;
    ok &= run_normalizer(&engine, NORMALIZER_FEATURE, &norm_host, "host-supplied shared blob")?;
    ok &= run_collator(&engine, COLLATOR_FEATURE, &coll_host, "host-supplied shared blob")?;
    ok &= run_normalizer(&engine, NORMALIZER_COMPOSED, &empty, "composed with shared data")?;
    ok &= run_collator(&engine, COLLATOR_COMPOSED, &empty, "composed with shared data")?;

    if !ok {
        anyhow::bail!("one or more checks failed");
    }
    println!("\nALL OK — collator and normalizer both run off ONE shared blob (NFD markers deduped)");
    Ok(())
}
