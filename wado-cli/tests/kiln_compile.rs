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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

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
        first.starts_with(b"\0asm"),
        "component must start with wasm magic"
    );
    assert!(first.len() > 100, "component must be non-trivial");
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
    assert_eq!(
        cache_files.len(),
        1,
        "exactly one cached component expected"
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
    let edited = format!(
        "{MINIMAL_GENERATOR}\nfn __touched() -> bool {{ return true; }}\n"
    );
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
        !second.is_empty() && second.starts_with(b"\0asm"),
        "second compile should produce valid component bytes"
    );
    let _ = first;
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
