//! Native wasmtime runtime for Kiln generators.
//!
//! Instantiates a compiled generator component against the `core:kiln/generator`
//! world, linking only the two host imports (`read-file` and `emit-diagnostic`).
//! Everything else — WASI, clocks, random, http — is unlinked; a generator that
//! imports any of those fails at link time, which is the determinism guarantee.
//!
//! See WEP 2026-04-12 (Kiln) §"Host-delegated execution".

use std::sync::{Arc, Mutex};

use anyhow::Result;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Engine, Store};

use wado_compiler::{
    CompilerHost, GeneratorDiagnostic, GeneratorDiagnosticLevel, GeneratorError,
    GeneratorInputFile, GeneratorOutputFile, GeneratorReadRecord, GeneratorRequest,
    GeneratorResponse, GeneratorRunnerError, GeneratorSourceSpan,
};

wasmtime::component::bindgen!({
    path: "../wado-compiler/lib/core/kiln/generator.wit",
    world: "generator",
    imports: { default: async },
    exports: { default: async },
});

use self::core::kiln::kiln_host;
use self::core::kiln::types as kiln_types;

/// Host policy for a single generator invocation.
///
/// `fuel` is the wasmtime fuel ceiling for the call; `0` means no ceiling
/// (the store is seeded with `u64::MAX`). The default is `0` because no
/// finite ceiling has yet proven to fit every Gale-sized grammar — the
/// 1 GiB initial pick tripped on `SQLite`. WEP 2026-04-12 (Kiln)
/// open-question #10 tracks exposing this as a `wado.toml` knob and
/// pairing it with a wall-clock deadline.
#[derive(Default)]
pub struct KilnRunPolicy {
    pub fuel: u64,
}

struct KilnHostState<H: CompilerHost> {
    host: Arc<H>,
    reads: Arc<Mutex<Vec<GeneratorReadRecord>>>,
    diagnostics: Arc<Mutex<Vec<GeneratorDiagnostic>>>,
}

impl<H: CompilerHost + 'static> kiln_host::Host for KilnHostState<H> {
    async fn read_file(&mut self, path: String) -> Result<String, kiln_host::HostError> {
        match self.host.load_source(&path).await {
            Ok(bytes) => {
                let hash = wado_compiler::kiln::content_hash(&bytes);
                self.reads.lock().unwrap().push(GeneratorReadRecord {
                    path: path.clone(),
                    content_hash: hash,
                });
                String::from_utf8(bytes)
                    .map_err(|e| kiln_host::HostError::Io(format!("{path}: not UTF-8: {e}")))
            }
            Err(wado_compiler::SourceError::NotFound { .. }) => Err(kiln_host::HostError::NotFound),
            Err(e) => Err(kiln_host::HostError::Io(e.to_string())),
        }
    }

    async fn emit_diagnostic(&mut self, diagnostic: kiln_host::Diagnostic) {
        self.diagnostics
            .lock()
            .unwrap()
            .push(lift_diagnostic(diagnostic));
    }
}

fn lift_diagnostic(d: kiln_host::Diagnostic) -> GeneratorDiagnostic {
    GeneratorDiagnostic {
        level: match d.level {
            kiln_host::DiagnosticLevel::Error => GeneratorDiagnosticLevel::Error,
            kiln_host::DiagnosticLevel::Warning => GeneratorDiagnosticLevel::Warning,
            kiln_host::DiagnosticLevel::Info => GeneratorDiagnosticLevel::Info,
            kiln_host::DiagnosticLevel::Hint => GeneratorDiagnosticLevel::Hint,
        },
        span: d.span.map(|s| GeneratorSourceSpan {
            path: s.path,
            byte_start: s.byte_start,
            byte_end: s.byte_end,
        }),
        message: d.message,
    }
}

fn lower_input_file(f: &GeneratorInputFile) -> kiln_types::InputFile {
    kiln_types::InputFile {
        path: f.path.clone(),
        content: f.content.clone(),
    }
}

fn lift_output_file(f: kiln_types::OutputFile) -> GeneratorOutputFile {
    GeneratorOutputFile {
        path: f.path,
        content: f.content,
        is_entry: f.is_entry,
    }
}

fn lift_error(e: kiln_types::Error) -> GeneratorError {
    match e {
        kiln_types::Error::InvalidSchema(msg) => GeneratorError::InvalidSchema(msg),
        kiln_types::Error::Unsupported(msg) => GeneratorError::Unsupported(msg),
        kiln_types::Error::Other(msg) => GeneratorError::Other(msg),
    }
}

/// Build a wasmtime [`Component`] from raw bytes. Cranelift-AOT
/// dominates here (~7s on a 432KB generator), so the host caches the
/// result across invocations that share the same bytes — see
/// [`crate::compiler_host::FilesystemCompilerHost`].
pub fn compile_component(
    engine: &Engine,
    component_wasm: &[u8],
) -> Result<Component, GeneratorRunnerError> {
    Component::from_binary(engine, component_wasm)
        .map_err(|e| GeneratorRunnerError::Host(format!("component compile: {e}")))
}

/// Instantiate a pre-built [`Component`] against the
/// `core:kiln/generator` world, invoke `generate`, and return the
/// response plus the files the generator read via `host::read-file`.
/// Always pass a [`Component`] obtained from the host's cache rather
/// than building one inline — see [`compile_component`].
pub async fn run_generator<H: CompilerHost + 'static>(
    engine: &Engine,
    host: Arc<H>,
    component: &Component,
    request: GeneratorRequest,
    policy: KilnRunPolicy,
) -> Result<(GeneratorResponse, Vec<GeneratorDiagnostic>), GeneratorRunnerError> {
    let reads = Arc::new(Mutex::new(Vec::<GeneratorReadRecord>::new()));
    let diagnostics = Arc::new(Mutex::new(Vec::<GeneratorDiagnostic>::new()));

    let state = KilnHostState {
        host: host.clone(),
        reads: reads.clone(),
        diagnostics: diagnostics.clone(),
    };

    // The kiln determinism guarantee (WEP 2026-04-12 §"Design
    // principles" #1) says the linker exposes only `core:kiln/kiln-
    // host`. The compiler handles the panic-path stderr elision
    // at codegen time so the generator component never imports WASI
    // in the first place.
    let mut linker: Linker<KilnHostState<H>> = Linker::new(engine);
    kiln_host::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)
        .map_err(|e| GeneratorRunnerError::Host(format!("linker setup: {e}")))?;

    let mut store = Store::new(engine, state);
    let fuel = if policy.fuel == 0 {
        u64::MAX
    } else {
        policy.fuel
    };
    store
        .set_fuel(fuel)
        .map_err(|e| GeneratorRunnerError::Host(format!("set fuel: {e}")))?;

    let generator = Generator::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| GeneratorRunnerError::Host(format!("instantiate: {e}")))?;

    let wit_request = kiln_types::RawRequest {
        primary: lower_input_file(&request.primary),
        inputs: request.inputs.iter().map(lower_input_file).collect(),
        options: request.options,
    };

    // Async exports in wasmtime bindgen take an `Accessor` rather than
    // `&mut Store`, so we drive the call through `run_concurrent`. The
    // outer Result combines wasmtime's runtime errors and the
    // generator's own typed `error` variant.
    let result = store
        .run_concurrent(async |accessor| generator.call_generate(accessor, wit_request).await)
        .await
        .map_err(|e| GeneratorRunnerError::Host(format!("generate call: {e}")))?
        .map_err(|e| GeneratorRunnerError::Host(format!("generate call: {e}")))?;

    // Hand the generator-emitted diagnostics back to the caller rather than
    // relaying them here: the only host reachable from this function is the
    // collect-only inner host, so relaying here would never print them. The
    // CLI host relays them through its printing wrapper instead (see
    // `FilesystemCompilerHost::run_generator`).
    let emitted: Vec<GeneratorDiagnostic> = diagnostics.lock().unwrap().drain(..).collect();

    match result {
        Ok(response) => Ok((
            GeneratorResponse {
                files: response.files.into_iter().map(lift_output_file).collect(),
                reads: std::mem::take(&mut *reads.lock().unwrap()),
            },
            emitted,
        )),
        Err(e) => Err(GeneratorRunnerError::Generator(lift_error(e))),
    }
}

pub(crate) fn relay_diagnostic<H: CompilerHost + ?Sized>(host: &H, diag: GeneratorDiagnostic) {
    use wado_compiler::{Code, Diagnostic, DiagnosticSpan, Severity};
    let severity = match diag.level {
        GeneratorDiagnosticLevel::Error => Severity::Error,
        GeneratorDiagnosticLevel::Warning => Severity::Warning,
        GeneratorDiagnosticLevel::Info => Severity::Info,
        GeneratorDiagnosticLevel::Hint => Severity::Info,
    };
    let span = diag.span.map(|s| DiagnosticSpan {
        file: s.path,
        line: 0,
        column: 0,
        end_line: None,
        end_column: None,
    });
    host.emit_diagnostic(Diagnostic {
        severity,
        code: Code::Log,
        message: diag.message,
        span,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::create_kiln_engine;

    #[test]
    fn malformed_component_is_host_error() {
        // Cranelift AOT now happens up-front in `compile_component`
        // (so the host can cache the result across invocations); this
        // is where a malformed module surfaces, not from
        // `run_generator`.
        let engine = create_kiln_engine(wasmtime::OptLevel::Speed).expect("engine");
        match compile_component(&engine, &[0, 1, 2, 3]) {
            Err(GeneratorRunnerError::Host(msg)) => {
                assert!(msg.contains("component compile"), "msg = {msg}");
            }
            Err(other) => panic!("expected Host error, got {other:?}"),
            Ok(_) => panic!("expected error on malformed bytes, got Ok"),
        }
    }

    #[test]
    fn sha256_known_vector_empty() {
        let digest = wado_compiler::kiln::content_hash(b"");
        let expected = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(digest, expected);
    }

    #[test]
    fn sha256_known_vector_abc() {
        let digest = wado_compiler::kiln::content_hash(b"abc");
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(digest, expected);
    }
}
