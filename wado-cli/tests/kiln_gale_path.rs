//! End-to-end Kiln-path test for the Gale generator.
//!
//! Exercises the `[build.generators.parser]` → `wado compile` route that
//! a real consumer takes when it references Gale as a Kiln generator:
//!
//! 1. Parse the kiln-consumer `wado.toml` under
//!    `tests/kiln-consumer/` and assert the manifest lowers to an
//!    `InvocationPath`-rooted `Plan`.
//! 2. Resolve the local Gale generator module via
//!    `CliGeneratorProvider::get_component` — compiling Gale end to end
//!    against the `core:kiln/generator` world — and assert the cached
//!    component bytes start with wasm magic.
//! 3. Re-run `get_component` and assert the second call hits the disk
//!    cache without re-invoking the inner compiler (via
//!    `CliGeneratorProvider::compile_count`).
//!
//! Full runtime invocation (instantiating the Gale component via
//! wasmtime and generating `calculator.wado` from the grammar) requires
//! the CM-type-instance-wrapping follow-up that removes
//! `skip_validation: true` on the generator compile. When that follow-up
//! lands, extend this test with a `run_pipeline`-based assertion that
//! diffs the generated output against `tests/golden/calculator.wado`.

#![allow(unused_crate_dependencies)]

use std::path::PathBuf;

use wado_cli::kiln_driver::{GeneratorProvider, plan};
use wado_cli::kiln_provider::CliGeneratorProvider;
use wado_compiler::kiln::GeneratorModule;

fn consumer_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at wado-cli/ when this test runs; step
    // up to the workspace root and descend into
    // package-gale/tests/kiln-consumer/.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("wado-cli has a parent workspace root")
        .join("package-gale")
        .join("tests")
        .join("kiln-consumer")
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn consumer_manifest_lowers_kiln_invocation_to_local_gale() {
    let root = consumer_root();
    let toml =
        std::fs::read_to_string(root.join("wado.toml")).expect("consumer manifest must exist");
    let manifest: wado_manifest::Manifest = toml
        .parse()
        .expect("consumer manifest must parse as wado.toml");

    let outcome = plan(&manifest, &root).expect("consumer plan must lower");
    assert_eq!(outcome.plan.order.len(), 1);
    let invocation = &outcome.plan.order[0];
    match &invocation.module {
        GeneratorModule::LocalPath(p) => {
            assert_eq!(
                p.as_str(),
                "../../src/generator.wado",
                "Gale generator path must be the explicit relative path from the \
                 consumer's manifest root — not a registry spec",
            );
        }
        other => panic!("expected LocalPath, got {other:?}"),
    }
    assert_eq!(invocation.from.as_str(), "grammar.g4");
    assert!(invocation.output_dir.as_str().starts_with("src/generated"));
}

#[test]
fn consumer_provider_compiles_gale_and_hits_cache_on_rerun() {
    let root = consumer_root();
    let provider = CliGeneratorProvider::new(root.clone());
    let module = GeneratorModule::LocalPath(wado_compiler::kiln::InvocationPath::normalize(
        "../../src/generator.wado",
    ));

    assert_eq!(provider.compile_count(), 0);

    let wasm = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("Gale generator must compile via the Kiln provider");
    assert!(
        wasm.starts_with(b"\0asm"),
        "Gale generator component must start with wasm magic"
    );
    assert!(wasm.len() > 1000, "Gale generator must be non-trivial");
    assert_eq!(
        provider.compile_count(),
        1,
        "first get_component should have compiled exactly once"
    );

    let wasm2 = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("second get_component must hit the on-disk cache");
    assert_eq!(wasm, wasm2);
    assert_eq!(
        provider.compile_count(),
        1,
        "cache hit must not re-invoke the inner compiler"
    );

    // Clean up the build/kiln/ artifact so the test is idempotent across
    // workspace-level builds. Best-effort; failure is harmless.
    let _ = std::fs::remove_dir_all(root.join("build"));
}
