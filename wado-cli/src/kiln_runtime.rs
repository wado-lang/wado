//! Native wasmtime runtime for Kiln generators.
//!
//! Instantiates a compiled generator component against the `wado:kiln/generator`
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
    path: "../wado-compiler/lib/wado/kiln/generator.wit",
    world: "generator",
    imports: { default: async },
    exports: { default: async },
});

use self::wado::kiln::host as kiln_host;
use self::wado::kiln::types as kiln_types;

/// Host policy for a single generator invocation.
///
/// Defaults per WEP open-question #10: 120s wall clock, 1 GiB of fuel.
pub struct KilnRunPolicy {
    pub fuel: u64,
}

impl Default for KilnRunPolicy {
    fn default() -> Self {
        Self {
            fuel: 1024 * 1024 * 1024,
        }
    }
}

struct KilnHostState<H: CompilerHost> {
    host: Arc<H>,
    reads: Arc<Mutex<Vec<GeneratorReadRecord>>>,
    diagnostics: Arc<Mutex<Vec<GeneratorDiagnostic>>>,
}

impl<H: CompilerHost + 'static> kiln_host::Host for KilnHostState<H> {
    async fn read_file(
        &mut self,
        path: String,
    ) -> Result<Vec<u8>, kiln_host::HostError> {
        match self.host.load_source(&path).await {
            Ok(bytes) => {
                let hash = sha256_digest(&bytes);
                self.reads.lock().unwrap().push(GeneratorReadRecord {
                    path: path.clone(),
                    content_hash: hash,
                });
                Ok(bytes)
            }
            Err(wado_compiler::SourceError::NotFound { .. }) => {
                Err(kiln_host::HostError::NotFound)
            }
            Err(e) => Err(kiln_host::HostError::Io(e.to_string())),
        }
    }

    async fn emit_diagnostic(&mut self, diagnostic: kiln_host::Diagnostic) {
        self.diagnostics.lock().unwrap().push(lift_diagnostic(diagnostic));
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

/// Instantiate `component_wasm` against the `wado:kiln/generator` world, call
/// its exported `generate`, and return the response plus the list of every
/// file the generator read via `host::read-file`.
pub async fn run_generator<H: CompilerHost + 'static>(
    engine: &Engine,
    host: Arc<H>,
    component_wasm: &[u8],
    request: GeneratorRequest,
    policy: KilnRunPolicy,
) -> Result<GeneratorResponse, GeneratorRunnerError> {
    let component = Component::from_binary(engine, component_wasm)
        .map_err(|e| GeneratorRunnerError::Host(format!("component compile: {e}")))?;

    let reads = Arc::new(Mutex::new(Vec::<GeneratorReadRecord>::new()));
    let diagnostics = Arc::new(Mutex::new(Vec::<GeneratorDiagnostic>::new()));

    let state = KilnHostState {
        host: host.clone(),
        reads: reads.clone(),
        diagnostics: diagnostics.clone(),
    };

    let mut linker: Linker<KilnHostState<H>> = Linker::new(engine);
    kiln_host::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)
        .map_err(|e| GeneratorRunnerError::Host(format!("linker setup: {e}")))?;

    let mut store = Store::new(engine, state);
    if policy.fuel > 0 {
        let _ = store.set_fuel(policy.fuel);
    }

    let generator = Generator::instantiate_async(&mut store, &component, &linker)
        .await
        .map_err(|e| GeneratorRunnerError::Host(format!("instantiate: {e}")))?;

    let wit_request = kiln_types::Request {
        primary: lower_input_file(&request.primary),
        inputs: request.inputs.iter().map(lower_input_file).collect(),
        options: request.options,
    };

    let result = generator
        .call_generate(&mut store, &wit_request)
        .await
        .map_err(|e| GeneratorRunnerError::Host(format!("generate call: {e}")))?;

    for diag in diagnostics.lock().unwrap().drain(..) {
        relay_diagnostic(host.as_ref(), diag);
    }

    match result {
        Ok(response) => Ok(GeneratorResponse {
            files: response.files.into_iter().map(lift_output_file).collect(),
            reads: std::mem::take(&mut *reads.lock().unwrap()),
        }),
        Err(e) => Err(GeneratorRunnerError::Generator(lift_error(e))),
    }
}

fn relay_diagnostic<H: CompilerHost + ?Sized>(host: &H, diag: GeneratorDiagnostic) {
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

fn sha256_digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ProfileMode, create_engine};
    use wado_compiler::{Diagnostic, SourceError};

    struct DummyHost;

    impl CompilerHost for DummyHost {
        async fn load_source(&self, _path: &str) -> Result<Vec<u8>, SourceError> {
            Err(SourceError::NotFound {
                path: "unused".to_string(),
            })
        }
        fn emit_diagnostic(&self, _diagnostic: Diagnostic) {}
    }

    #[test]
    fn malformed_component_is_host_error() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let engine = create_engine(wasmtime::OptLevel::Speed, &ProfileMode::None)
                    .expect("engine");
                let host = Arc::new(DummyHost);
                let request = GeneratorRequest {
                    primary: GeneratorInputFile {
                        path: "schema.proto".to_string(),
                        content: b"syntax = \"proto3\";".to_vec(),
                    },
                    inputs: vec![],
                    options: vec![],
                };
                let result = run_generator(
                    &engine,
                    host,
                    &[0, 1, 2, 3],
                    request,
                    KilnRunPolicy::default(),
                )
                .await;
                match result {
                    Err(GeneratorRunnerError::Host(msg)) => {
                        assert!(msg.contains("component compile"), "msg = {msg}");
                    }
                    other => panic!("expected Host error, got {other:?}"),
                }
            });
    }

    #[test]
    fn sha256_known_vector_empty() {
        let digest = sha256_digest(b"");
        let expected = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(digest, expected);
    }

    #[test]
    fn sha256_known_vector_abc() {
        let digest = sha256_digest(b"abc");
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(digest, expected);
    }
}
