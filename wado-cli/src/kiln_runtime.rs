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
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize()
}

struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    total_len: u64,
}

impl Sha256 {
    const H0: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const K: [u32; 64] = [
        0x428a_2f98, 0x7137_4491, 0xb5c0_fbcf, 0xe9b5_dba5, 0x3956_c25b, 0x59f1_11f1,
        0x923f_82a4, 0xab1c_5ed5, 0xd807_aa98, 0x1283_5b01, 0x2431_85be, 0x550c_7dc3,
        0x72be_5d74, 0x80de_b1fe, 0x9bdc_06a7, 0xc19b_f174, 0xe49b_69c1, 0xefbe_4786,
        0x0fc1_9dc6, 0x240c_a1cc, 0x2de9_2c6f, 0x4a74_84aa, 0x5cb0_a9dc, 0x76f9_88da,
        0x983e_5152, 0xa831_c66d, 0xb003_27c8, 0xbf59_7fc7, 0xc6e0_0bf3, 0xd5a7_9147,
        0x06ca_6351, 0x1429_2967, 0x27b7_0a85, 0x2e1b_2138, 0x4d2c_6dfc, 0x5338_0d13,
        0x650a_7354, 0x766a_0abb, 0x81c2_c92e, 0x9272_2c85, 0xa2bf_e8a1, 0xa81a_664b,
        0xc24b_8b70, 0xc76c_51a3, 0xd192_e819, 0xd699_0624, 0xf40e_3585, 0x106a_a070,
        0x19a4_c116, 0x1e37_6c08, 0x2748_774c, 0x34b0_bcb5, 0x391c_0cb3, 0x4ed8_aa4a,
        0x5b9c_ca4f, 0x682e_6ff3, 0x748f_82ee, 0x78a5_636f, 0x84c8_7814, 0x8cc7_0208,
        0x90be_fffa, 0xa450_6ceb, 0xbef9_a3f7, 0xc671_78f2,
    ];

    fn new() -> Self {
        Self {
            state: Self::H0,
            buffer: Vec::with_capacity(64),
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if !self.buffer.is_empty() {
            let need = 64 - self.buffer.len();
            let take = need.min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() == 64 {
                let block: [u8; 64] = self.buffer.as_slice().try_into().unwrap();
                self.compress(&block);
                self.buffer.clear();
            }
        }
        while data.len() >= 64 {
            let block: [u8; 64] = data[..64].try_into().unwrap();
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer.extend_from_slice(data);
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bit_len.to_be_bytes());
        let trailing = std::mem::take(&mut self.buffer);
        for chunk in trailing.chunks(64) {
            let block: [u8; 64] = chunk.try_into().unwrap();
            self.compress(&block);
        }
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
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
