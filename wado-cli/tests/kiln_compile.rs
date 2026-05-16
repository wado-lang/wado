//! End-to-end test for `CliGeneratorProvider::get_component` — compiles a
//! synthetic single-file Kiln generator through the real wado-compiler
//! pipeline and verifies:
//!
//! 1. First run emits a component (`\0asm` magic) to
//!    `build/kiln/generators/<stable-id>.wasm`.
//! 2. Second run returns identical bytes and does **not** re-invoke the
//!    inner compiler. Observed via `CliGeneratorProvider::compile_count`
//!    rather than filesystem mtime, which avoids sleep + tmpfs/nfs
//!    resolution flakiness.
//! 3. Missing source path surfaces `ProviderError::Internal`.
//!
//! See WEP 2026-04-12 §"M6.7" (the `wado-cli/tests/kiln_compile.rs` bullet).

#![allow(unused_crate_dependencies)]

use std::path::PathBuf;

use wado_cli::kiln_driver::{GeneratorProvider, ProviderError};
use wado_cli::kiln_provider::{CACHE_DIR, CliGeneratorProvider};
use wado_compiler::kiln::{GeneratorModule, InvocationPath};

mod common;
use common::runtime;

const MINIMAL_GENERATOR: &str = r#"
use { RawRequest, Response, Error, bind_request } from "core:kiln";

pub struct Options {
    pub verbose: bool,
}

export fn generate(raw: RawRequest) -> Result<Response, Error> {
    let req = match bind_request::<Options>(raw) {
        Ok(r) => r,
        Err(e) => return Result::Err(e),
    };
    let _ = req.options.verbose;
    return Result::Ok(Response { files: [] });
}
"#;

/// v2-ergonomic form: author writes `fn generate(req: Request<Options>)`
/// directly and the compiler's
/// `kiln::import_check::inject_kiln_request_adapter` phase rewrites it
/// to the internal `RawRequest + bind_request?` shape before analyze
/// runs. Note we don't import `bind_request` or `RawRequest` — the
/// phase extends the existing `use` automatically.
const ADAPTER_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

pub struct Options {
    pub verbose: bool,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.options.verbose;
    return Result::Ok(Response { files: [] });
}
"#;

fn unique_tmp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wado-{label}-{}", std::process::id()))
}

#[test]
fn first_run_compiles_second_run_hits_cache() {
    let tmp = unique_tmp("kiln-compile-cache");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let gen_path = tmp.join("my_generator.wado");
    std::fs::write(&gen_path, MINIMAL_GENERATOR).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./my_generator.wado"));

    assert_eq!(
        provider.compile_count(),
        0,
        "provider starts with no compiles"
    );

    let first = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("first compile should succeed");
    assert!(
        first.bytes.starts_with(b"\0asm"),
        "component must start with wasm magic"
    );
    assert!(first.bytes.len() > 100, "component must be non-trivial");
    assert_eq!(
        provider.compile_count(),
        1,
        "first call should have run the inner compiler exactly once"
    );

    // Verify the cache artifact landed where the stable-id scheme
    // expects.
    let cache_dir = tmp.join(CACHE_DIR);
    let cache_files: Vec<PathBuf> = std::fs::read_dir(&cache_dir)
        .expect("cache dir should exist after first compile")
        .filter_map(|r| r.ok().map(|e| e.path()))
        .collect();
    let wasm_files: Vec<&PathBuf> = cache_files
        .iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "wasm"))
        .collect();
    assert_eq!(wasm_files.len(), 1, "exactly one cached component expected");
    // The compile path also persists a `.sources.json` sidecar listing
    // the transitive `.wado` closure for cache invalidation.
    let sidecar_files: Vec<&PathBuf> = cache_files
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".sources.json"))
        .collect();
    assert_eq!(
        sidecar_files.len(),
        1,
        "exactly one cached sources sidecar expected"
    );

    // Second call: same source, same stable-id → must hit the cache,
    // return identical bytes, and leave the compile counter untouched.
    // No sleep needed — `compile_count` is the direct observation
    // point, not filesystem mtime.
    let second = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("second compile should succeed from cache");
    assert_eq!(first, second, "cache hit must return identical bytes");
    assert_eq!(
        provider.compile_count(),
        1,
        "cache hit must not re-invoke the inner compiler"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn source_edit_invalidates_cache() {
    let tmp = unique_tmp("kiln-compile-invalidate");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let gen_path = tmp.join("my_generator.wado");
    std::fs::write(&gen_path, MINIMAL_GENERATOR).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./my_generator.wado"));

    let first = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("first compile should succeed");
    assert_eq!(provider.compile_count(), 1);

    // Rewrite the source. The stable-id is `sha256(path || content)`,
    // so a content change picks a different cache key and must force
    // a fresh compile.
    let edited = format!("{MINIMAL_GENERATOR}\nfn __touched() -> bool {{ return true; }}\n");
    std::fs::write(&gen_path, edited).unwrap();

    let second = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("second compile after edit should succeed");
    assert_eq!(
        provider.compile_count(),
        2,
        "content change must miss cache and re-invoke the compiler"
    );
    // The compiler may happen to produce byte-identical output after a
    // trivial edit (e.g. DCE drops the added dead function). Still
    // verify it produced something on second compile, but don't require
    // bytewise divergence.
    assert!(
        !second.bytes.is_empty() && second.bytes.starts_with(b"\0asm"),
        "second compile should produce valid component bytes"
    );
    let _ = first;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn adapter_generator_compiles_like_raw_request_generator() {
    let tmp = unique_tmp("kiln-compile-adapter");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let gen_path = tmp.join("adapter_generator.wado");
    std::fs::write(&gen_path, ADAPTER_GENERATOR).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./adapter_generator.wado"));

    let component = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("adapter-form generator must compile");
    assert!(
        component.bytes.starts_with(b"\0asm"),
        "adapter generator must produce a valid wasm component"
    );
    assert!(
        component.bytes.len() > 100,
        "adapter generator must produce a non-trivial component"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn missing_local_path_surfaces_internal() {
    let provider = CliGeneratorProvider::new(PathBuf::from("/nonexistent-wado-kiln-root"));
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./nowhere.wado"));
    let err = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect_err("missing path should fail");
    match err {
        ProviderError::Internal { message } => {
            assert!(
                message.contains("does not exist"),
                "expected path-not-found message, got: {message}"
            );
        }
        _ => panic!("expected ProviderError::Internal, got {err:?}"),
    }
}

#[test]
fn transitive_dep_edit_invalidates_cache() {
    // Regression: the WASM cache key was previously
    // `sha256(path || entry-file content)`, so an edit to a `.wado`
    // file imported (transitively) by the generator entry left the
    // cached bytes untouched. Local generator authors hit this
    // constantly while iterating on a parser-gen library — the next
    // `wado test` would happily reuse the previous WASM.
    //
    // The fix records every `.wado` file the inner compile loaded
    // into a `<stable_id>.sources.json` sidecar; cache hits re-hash
    // those entries and miss when any drift.
    let tmp = unique_tmp("kiln-compile-transitive");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Two-file generator: entry imports a sibling helper and uses one
    // of its functions so the helper is part of the closure (not
    // dead-import-stripped).
    let helper_path = tmp.join("helper.wado");
    std::fs::write(
        &helper_path,
        r"pub fn answer() -> i32 { return 42; }
",
    )
    .unwrap();
    let entry_src = r#"
use { RawRequest, Response, Error, bind_request } from "core:kiln";
use { answer } from "./helper.wado";

pub struct Options {
    pub verbose: bool,
}

export fn generate(raw: RawRequest) -> Result<Response, Error> {
    let req = match bind_request::<Options>(raw) {
        Ok(r) => r,
        Err(e) => return Result::Err(e),
    };
    let _ = req.options.verbose;
    let _ = answer();
    return Result::Ok(Response { files: [] });
}
"#;
    let gen_path = tmp.join("entry.wado");
    std::fs::write(&gen_path, entry_src).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./entry.wado"));

    let _first = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("first compile should succeed");
    assert_eq!(provider.compile_count(), 1);

    // Cache hit on the entry file alone — the entry's SHA-256 is
    // unchanged, so the v1 stable-id keeps pointing at the same WASM.
    let _second = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("second compile should succeed");
    assert_eq!(
        provider.compile_count(),
        1,
        "cache hit when no source drifted"
    );

    // Now edit the transitive helper. Without sidecar validation the
    // cache would hit again (entry SHA-256 unchanged); with the fix the
    // sidecar's recorded hash for `helper.wado` no longer matches the
    // file on disk and the provider falls back to a fresh compile.
    std::fs::write(
        &helper_path,
        r"pub fn answer() -> i32 { return 43; }
",
    )
    .unwrap();

    let _third = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("third compile should succeed");
    assert_eq!(
        provider.compile_count(),
        2,
        "transitive dep edit must invalidate the WASM cache"
    );

    // After the rebuild the new sidecar should reflect the latest
    // helper hash, so a fourth call hits cache cleanly.
    let _fourth = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("fourth compile should succeed");
    assert_eq!(
        provider.compile_count(),
        2,
        "post-rebuild cache hit when no further edit"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn fresh_recompile_produces_identical_source_hash() {
    // Regression for issue #1059. The contract is: same generator source
    // closure (entry + transitive imports) must produce the same
    // `source_hash`, regardless of cache state.
    let tmp = unique_tmp("kiln-source-hash-determinism");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let helper_path = tmp.join("helper.wado");
    let helper_src = "pub fn answer() -> i32 { return 42; }\n";
    std::fs::write(&helper_path, helper_src).unwrap();

    let entry_src = r#"
use { RawRequest, Response, Error, bind_request } from "core:kiln";
use { answer } from "./helper.wado";

pub struct Options {
    pub verbose: bool,
}

export fn generate(raw: RawRequest) -> Result<Response, Error> {
    let req = match bind_request::<Options>(raw) {
        Ok(r) => r,
        Err(e) => return Result::Err(e),
    };
    let _ = req.options.verbose;
    let _ = answer();
    return Result::Ok(Response { files: [] });
}
"#;
    let gen_path = tmp.join("entry.wado");
    std::fs::write(&gen_path, entry_src).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./entry.wado"));

    // Run 1: cold cache.
    let first = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("first compile should succeed");
    assert_eq!(provider.compile_count(), 1);
    assert!(
        !first.source_hash.is_empty(),
        "source hash must be recorded"
    );

    // Wipe the build cache (WASM + sidecar + descriptor) so the next
    // call is a true cold compile, not a sidecar-revalidate fast path.
    let _ = std::fs::remove_dir_all(tmp.join("build"));

    // Run 2: same source bytes, fresh compile.
    let second = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("second compile should succeed");
    assert_eq!(
        provider.compile_count(),
        2,
        "wiping the cache must force a fresh compile"
    );
    assert_eq!(
        first.source_hash, second.source_hash,
        "byte-identical source closure must produce a byte-identical source_hash \
         across runs (issue #1059: kiln json otherwise churns its `generator_source_hash` \
         field on every cold compile)"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn docstring_only_edit_preserves_source_hash() {
    // The point of switching the per-file hash from raw bytes to the
    // canonical token stream (issue #1059): docstring and comment
    // edits must not change the generator source hash, so they do not
    // churn `generator_source_hash` in every consumer's kiln.json.
    let tmp = unique_tmp("kiln-source-hash-doc-only");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let helper_path = tmp.join("helper.wado");
    std::fs::write(
        &helper_path,
        "/// Original docstring.\npub fn answer() -> i32 { return 42; }\n",
    )
    .unwrap();

    let entry_src = r#"
use { RawRequest, Response, Error, bind_request } from "core:kiln";
use { answer } from "./helper.wado";

pub struct Options {
    pub verbose: bool,
}

export fn generate(raw: RawRequest) -> Result<Response, Error> {
    let req = match bind_request::<Options>(raw) {
        Ok(r) => r,
        Err(e) => return Result::Err(e),
    };
    let _ = req.options.verbose;
    let _ = answer();
    return Result::Ok(Response { files: [] });
}
"#;
    let gen_path = tmp.join("entry.wado");
    std::fs::write(&gen_path, entry_src).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./entry.wado"));

    let baseline = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("baseline compile should succeed");

    // Edit only the docstring. Comment-only/whitespace-only edits in
    // `.wado` files MUST NOT change the source hash now that hashing
    // routes through the canonical token stream.
    std::fs::write(
        &helper_path,
        "//! New module-level doc.\n/// Tweaked docstring with extra detail.\n// Plus a stray line comment.\npub fn answer() -> i32 { return 42; }\n",
    )
    .unwrap();
    let after_doc = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("post-doc-edit compile should succeed");
    assert_eq!(
        baseline.source_hash, after_doc.source_hash,
        "docstring/comment-only edits must not change the source hash"
    );

    // Whitespace/formatting changes also must be invisible to the hash.
    std::fs::write(
        &helper_path,
        "//! New module-level doc.\n/// Tweaked docstring with extra detail.\n// Plus a stray line comment.\npub fn answer() -> i32 {\n    return 42;\n}\n",
    )
    .unwrap();
    let after_format = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("post-format compile should succeed");
    assert_eq!(
        baseline.source_hash, after_format.source_hash,
        "whitespace-only edits must not change the source hash"
    );

    // A real semantic edit (return value change) MUST still bump the hash.
    std::fs::write(
        &helper_path,
        "/// Original docstring.\npub fn answer() -> i32 { return 99; }\n",
    )
    .unwrap();
    let after_semantic = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("post-semantic-edit compile should succeed");
    assert_ne!(
        baseline.source_hash, after_semantic.source_hash,
        "a real source change must still bump the hash — otherwise the cache \
         would silently reuse stale generator output"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn transitive_edit_then_revert_restores_source_hash() {
    // Tighter regression for issue #1059. The user's exact scenario:
    // start at HEAD, edit a transitive `.wado` file, run tests (which
    // regenerate kiln json), revert the edit, run tests again — the
    // resulting `generator_source_hash` must match the HEAD value.
    let tmp = unique_tmp("kiln-source-hash-revert");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let helper_path = tmp.join("helper.wado");
    let helper_original = "pub fn answer() -> i32 { return 42; }\n";
    std::fs::write(&helper_path, helper_original).unwrap();

    let entry_src = r#"
use { RawRequest, Response, Error, bind_request } from "core:kiln";
use { answer } from "./helper.wado";

pub struct Options {
    pub verbose: bool,
}

export fn generate(raw: RawRequest) -> Result<Response, Error> {
    let req = match bind_request::<Options>(raw) {
        Ok(r) => r,
        Err(e) => return Result::Err(e),
    };
    let _ = req.options.verbose;
    let _ = answer();
    return Result::Ok(Response { files: [] });
}
"#;
    let gen_path = tmp.join("entry.wado");
    std::fs::write(&gen_path, entry_src).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::LocalPath(InvocationPath::normalize("./entry.wado"));

    // Step 1: compile at "HEAD" — capture the baseline hash.
    let head = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("HEAD compile should succeed");
    let baseline = head.source_hash;

    // Step 2: edit the transitive helper to a different body.
    std::fs::write(&helper_path, "pub fn answer() -> i32 { return 99; }\n").unwrap();
    let edited = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("edited compile should succeed");
    assert_ne!(
        edited.source_hash, baseline,
        "the edit must change the source hash, otherwise this test does not exercise \
         the revert path"
    );

    // Step 3: revert the helper to its original byte content.
    std::fs::write(&helper_path, helper_original).unwrap();
    let reverted = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect("post-revert compile should succeed");
    assert_eq!(
        reverted.source_hash, baseline,
        "after reverting the transitive file, the source hash must match the HEAD \
         baseline (issue #1059)"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn spec_module_surfaces_unsupported() {
    let tmp = unique_tmp("kiln-compile-spec");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let provider = CliGeneratorProvider::new(tmp.clone());
    let module = GeneratorModule::Spec("example:proto-codegen@1.2.3".to_string());
    let err = runtime()
        .block_on(async { provider.get_component(&module).await })
        .expect_err("spec module should fail until registry support lands");
    match err {
        ProviderError::Unsupported { message } => {
            assert!(
                message.contains("registry") || message.contains("package spec"),
                "expected spec-module unsupported message, got: {message}"
            );
        }
        _ => panic!("expected ProviderError::Unsupported, got {err:?}"),
    }

    let _ = std::fs::remove_dir_all(&tmp);
}
