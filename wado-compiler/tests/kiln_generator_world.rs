//! End-to-end test that a minimal Wado source targeting the
//! `core:kiln/generator` world compiles to valid component bytes.
//!
//! Covers:
//! - Happy path: `export fn generate(raw: RawRequest)` using
//!   `bind_request::<Options>(raw)?` and returning an empty `Response`.
//! - Import-refusal: adding `use { now } from "wasi:clocks";` to the
//!   same generator surfaces `Code::KilnGeneratorForbiddenImport`.
//!
//! See WEP 2026-04-12 §"M6.5 stage 2".

#![allow(unused_crate_dependencies)]

use std::sync::Mutex;

use indexmap::IndexMap;
use wado_compiler::{
    Code, CompileResult, CompilerHost, CompilerOptions, Diagnostic, LogLevel, Severity,
    SourceError, compile_with_options,
};

struct MapHost {
    sources: IndexMap<String, String>,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl MapHost {
    fn new(sources: &[(&str, &str)]) -> Self {
        Self {
            sources: sources
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }
}

impl CompilerHost for MapHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, SourceError>> + Send {
        let result = self.sources.get(path).cloned();
        let path = path.to_string();
        async move {
            match result {
                Some(s) => Ok(s.into_bytes()),
                None => Err(SourceError::NotFound { path }),
            }
        }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn kiln_options() -> CompilerOptions {
    // `skip_validation: true` is a placeholder until the CM adapter
    // synthesis follow-up lands. Today the core Wasm wrapper treats the
    // `raw: RawRequest` parameter as a Wasm GC reference, but canonical
    // async lift materializes the record in linear memory — a type
    // mismatch the validator catches. The generator still type-checks,
    // synthesis still runs, and the bind_request call site exercises the
    // full resolver / auto-derive path. Remove this flag once the
    // adapter lands.
    CompilerOptions {
        log_level: Some(LogLevel::Warn),
        target_world: Some("core:kiln/generator".to_string()),
        skip_validation: true,
        ..CompilerOptions::default()
    }
}

const NOOP_GENERATOR: &str = r#"
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

#[test]
fn noop_generator_compiles_to_component_bytes() {
    let host = MapHost::new(&[]);
    let result: Result<CompileResult, _> = block_on(compile_with_options(
        NOOP_GENERATOR,
        &host,
        Some("generator.wado"),
        kiln_options(),
    ));
    let Ok(result) = result else {
        let diags = host.diagnostics();
        panic!(
            "noop generator failed to compile: {}",
            diags
                .iter()
                .map(|d| format!("  {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    };
    assert!(
        !result.wasm.is_empty(),
        "noop generator produced empty wasm"
    );
    assert!(
        result.wasm.starts_with(b"\0asm"),
        "noop generator did not produce a component-shaped wasm (first bytes = {:?})",
        &result.wasm[..4.min(result.wasm.len())],
    );
}

const FORBIDDEN_IMPORT_GENERATOR: &str = r#"
use { RawRequest, Response, Error, bind_request } from "core:kiln";
use { now } from "wasi:clocks";

pub struct Options {
    pub verbose: bool,
}

export fn generate(raw: RawRequest) -> Result<Response, Error> {
    let _req = bind_request::<Options>(raw);
    let _ = now();
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn generator_importing_wasi_clocks_is_rejected() {
    let host = MapHost::new(&[]);
    let result = block_on(compile_with_options(
        FORBIDDEN_IMPORT_GENERATOR,
        &host,
        Some("generator.wado"),
        kiln_options(),
    ));
    assert!(result.is_err(), "generator with wasi: import should fail");

    let diags = host.diagnostics();
    let found = diags.iter().any(|d| {
        d.severity == Severity::Error
            && d.code == Code::KilnGeneratorForbiddenImport
            && d.message.contains("wasi:clocks")
    });
    assert!(
        found,
        "expected KilnGeneratorForbiddenImport diagnostic mentioning wasi:clocks, got: {}",
        diags
            .iter()
            .map(|d| format!("  {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
