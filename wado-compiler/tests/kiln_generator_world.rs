//! End-to-end test that a minimal Wado source targeting the
//! `core:kiln/generator` world compiles to valid component bytes.
//!
//! Covers:
//! - Happy path (no options): `export fn generate(req: Request)` returning
//!   an empty `Response`.
//! - Typed options: `export fn generate(req: Request<Options>)` produces a
//!   valid component whose `generate` carries `Options` as a typed argument.
//! - Import-refusal: adding `use { now } from "wasi:clocks";` to a
//!   generator surfaces `Code::KilnGeneratorForbiddenImport`.
//!
//! See WEP 2026-04-12 §"Protocol revision v0.3".

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
    // The v0.3 typed-request adapter produces a valid component, so no
    // `skip_validation` is needed — the `generate(primary, inputs, options)`
    // lift/lower round-trips through the CM ABI cleanly.
    CompilerOptions {
        log_level: Some(LogLevel::Warn),
        target_world: Some("core:kiln/generator".to_string()),
        ..CompilerOptions::default()
    }
}

const NOOP_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
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

const TYPED_OPTIONS_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

pub struct Options {
    pub highlight: bool,
    pub trace: bool,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let _ = req.primary.path;
    let _ = req.options.highlight;
    let _ = req.options.trace;
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn typed_options_generator_compiles_to_valid_component() {
    let host = MapHost::new(&[]);
    // No `skip_validation`: the v0.3 typed-options `generate(primary, inputs,
    // options)` shape must produce a valid component (unlike the old
    // `raw-request` GC-reference mismatch).
    let options = CompilerOptions {
        log_level: Some(LogLevel::Warn),
        target_world: Some("core:kiln/generator".to_string()),
        ..CompilerOptions::default()
    };
    let result = block_on(compile_with_options(
        TYPED_OPTIONS_GENERATOR,
        &host,
        Some("generator.wado"),
        options,
    ));
    let Ok(result) = result else {
        let diags = host.diagnostics();
        panic!(
            "typed-options generator failed to compile:\n{}",
            diags
                .iter()
                .map(|d| format!("  {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    };
    assert!(result.wasm.starts_with(b"\0asm"), "not component-shaped");
}

/// A generator may declare helper `export fn`s beside `generate`. Only
/// `generate` is the world contract, so a non-`generate` export (here a plain
/// `u32`-returning function) must not be force-routed through the async
/// task-return result binding, which would emit an invalid component.
const MULTI_EXPORT_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";

export fn helper() -> u32 {
    return 7;
}

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
    let _ = helper();
    return Result::Ok(Response { files: [] });
}
"#;

#[test]
fn generator_with_extra_export_compiles_to_valid_component() {
    let host = MapHost::new(&[]);
    let options = CompilerOptions {
        log_level: Some(LogLevel::Warn),
        target_world: Some("core:kiln/generator".to_string()),
        ..CompilerOptions::default()
    };
    let result = block_on(compile_with_options(
        MULTI_EXPORT_GENERATOR,
        &host,
        Some("generator.wado"),
        options,
    ));
    let Ok(result) = result else {
        let diags = host.diagnostics();
        panic!(
            "multi-export generator failed to compile:\n{}",
            diags
                .iter()
                .map(|d| format!("  {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    };
    assert!(result.wasm.starts_with(b"\0asm"), "not component-shaped");
}

const FORBIDDEN_IMPORT_GENERATOR: &str = r#"
use { Request, Response, Error } from "core:kiln";
use { now } from "wasi:clocks";

export fn generate(req: Request) -> Result<Response, Error> {
    let _ = req.primary.path;
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
