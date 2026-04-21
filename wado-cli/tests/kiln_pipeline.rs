//! Integration tests for the Kiln pipeline driver.
//!
//! Exercises `wado_cli::kiln_driver::run_pipeline` through the crate's
//! public API, using a static in-memory generator provider and a mock
//! `CompilerHost`. This complements the unit tests inside the module
//! by proving the orchestrator is usable from outside the crate.

use std::path::Path;
use std::sync::Mutex;

use indexmap::IndexMap;
use wado_cli::kiln_driver::{GeneratorProvider, PipelineOutcome, ProviderError, run_pipeline};
use wado_compiler::compiler_host::{
    CompilerHost, Diagnostic, GeneratorOutputFile, GeneratorReadRecord, GeneratorRequest,
    GeneratorResponse, GeneratorRunnerError, SourceError,
};
use wado_compiler::kiln::GeneratorModule;
use wado_manifest::{LockFile, Manifest};

struct StubProvider {
    bytes: Vec<u8>,
    calls: Mutex<u32>,
}

impl StubProvider {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            calls: Mutex::new(0),
        }
    }
}

impl GeneratorProvider for StubProvider {
    async fn get_component(&self, _: &GeneratorModule) -> Result<Vec<u8>, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        Ok(self.bytes.clone())
    }
}

struct StubHost {
    sources: IndexMap<String, Vec<u8>>,
    responses: Mutex<IndexMap<String, GeneratorResponse>>,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl StubHost {
    fn new(sources: &[(&str, &[u8])], responses: Vec<(&str, GeneratorResponse)>) -> Self {
        Self {
            sources: sources
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_vec()))
                .collect(),
            responses: Mutex::new(
                responses
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            ),
            diagnostics: Mutex::new(Vec::new()),
        }
    }

    fn take_diagnostics(&self) -> Vec<Diagnostic> {
        std::mem::take(&mut *self.diagnostics.lock().unwrap())
    }
}

impl CompilerHost for StubHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.sources
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }

    fn emit_diagnostic(&self, d: Diagnostic) {
        self.diagnostics.lock().unwrap().push(d);
    }

    async fn run_generator(
        &self,
        _wasm: &[u8],
        request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        let mut map = self.responses.lock().unwrap();
        map.shift_remove(&request.primary.path).ok_or_else(|| {
            GeneratorRunnerError::Host(format!(
                "stub host: no response registered for {}",
                request.primary.path
            ))
        })
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn run(
    manifest: &Manifest,
    root: &Path,
    host: &StubHost,
    provider: &StubProvider,
) -> PipelineOutcome {
    runtime()
        .block_on(async { run_pipeline(manifest, root, host, provider).await })
        .unwrap()
}

fn manifest_with_two_disjoint_generators() -> Manifest {
    let toml_str = r#"
[package]
name = "host"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }
graphql = { package = "ns:graphql", version = "^1.0.0" }

[build.generators.alpha]
module = "ns:proto@1.0.0"
from = "./alpha.proto"

[build.generators.beta]
module = "ns:graphql@1.0.0"
from = "./beta.graphql"
"#;
    toml_str.parse().unwrap()
}

fn simple_response(path: &str) -> GeneratorResponse {
    GeneratorResponse {
        files: vec![GeneratorOutputFile {
            path: path.to_string(),
            content: format!("pub fn from_{}() {{}}\n", path.replace('.', "_")),
            is_entry: true,
        }],
        reads: Vec::<GeneratorReadRecord>::new(),
    }
}

#[test]
fn end_to_end_two_generators_execute_cache_and_reuse() {
    let tmp = tempfile::tempdir().unwrap();
    let m = manifest_with_two_disjoint_generators();

    let host1 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![
            ("alpha.proto", simple_response("alpha.wado")),
            ("beta.graphql", simple_response("beta.wado")),
        ],
    );
    let provider1 = StubProvider::new(b"component-bytes".to_vec());

    let outcome = run(&m, tmp.path(), &host1, &provider1);
    assert_eq!(outcome.executed.len(), 2);
    assert!(outcome.executed.contains(&"alpha".to_string()));
    assert!(outcome.executed.contains(&"beta".to_string()));
    assert!(outcome.cached.is_empty());

    assert!(tmp.path().join("build/kiln/alpha/alpha.wado").exists());
    assert!(tmp.path().join("build/kiln/beta/beta.wado").exists());

    let lock_text = std::fs::read_to_string(tmp.path().join("wado.lock")).unwrap();
    let lock: LockFile = lock_text.parse().unwrap();
    assert_eq!(lock.generator_cache.len(), 2);
    let names: Vec<&str> = lock
        .generator_cache
        .iter()
        .map(|e| e.invocation.as_str())
        .collect();
    assert_eq!(names, vec!["alpha", "beta"]);

    let host2 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![],
    );
    let provider2 = StubProvider::new(b"component-bytes".to_vec());

    let outcome2 = run(&m, tmp.path(), &host2, &provider2);
    assert_eq!(outcome2.cached.len(), 2);
    assert!(outcome2.executed.is_empty());
    assert_eq!(*provider2.calls.lock().unwrap(), 0);
}

#[test]
fn lockfile_entries_follow_manifest_declaration_order() {
    let tmp = tempfile::tempdir().unwrap();
    let toml_str = r#"
[package]
name = "host"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
zebra = { package = "ns:zebra", version = "^1.0.0" }
alpha = { package = "ns:alpha", version = "^1.0.0" }
mango = { package = "ns:mango", version = "^1.0.0" }

[build.generators.zebra]
module = "ns:zebra@1.0.0"
from = "./z.schema"

[build.generators.alpha]
module = "ns:alpha@1.0.0"
from = "./a.schema"

[build.generators.mango]
module = "ns:mango@1.0.0"
from = "./m.schema"
"#;
    let m: Manifest = toml_str.parse().unwrap();

    let host = StubHost::new(
        &[
            ("z.schema", b"z-schema"),
            ("a.schema", b"a-schema"),
            ("m.schema", b"m-schema"),
        ],
        vec![
            ("z.schema", simple_response("z.wado")),
            ("a.schema", simple_response("a.wado")),
            ("m.schema", simple_response("m.wado")),
        ],
    );
    let provider = StubProvider::new(b"component-bytes".to_vec());
    run(&m, tmp.path(), &host, &provider);

    let lock_text = std::fs::read_to_string(tmp.path().join("wado.lock")).unwrap();
    let lock: LockFile = lock_text.parse().unwrap();
    let names: Vec<&str> = lock
        .generator_cache
        .iter()
        .map(|e| e.invocation.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["zebra", "alpha", "mango"],
        "lockfile must follow manifest declaration order, not alphabetical"
    );
    let z_pos = lock_text.find("invocation = \"zebra\"").unwrap();
    let a_pos = lock_text.find("invocation = \"alpha\"").unwrap();
    let m_pos = lock_text.find("invocation = \"mango\"").unwrap();
    assert!(z_pos < a_pos && a_pos < m_pos);
}

#[test]
fn end_to_end_input_change_invalidates_exactly_that_invocation() {
    let tmp = tempfile::tempdir().unwrap();
    let m = manifest_with_two_disjoint_generators();

    let host1 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![
            ("alpha.proto", simple_response("alpha.wado")),
            ("beta.graphql", simple_response("beta.wado")),
        ],
    );
    let provider1 = StubProvider::new(b"component-bytes".to_vec());
    run(&m, tmp.path(), &host1, &provider1);

    let host2 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema-EDITED"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![("alpha.proto", simple_response("alpha.wado"))],
    );
    let provider2 = StubProvider::new(b"component-bytes".to_vec());

    let outcome = run(&m, tmp.path(), &host2, &provider2);
    assert_eq!(outcome.executed, vec!["alpha".to_string()]);
    assert_eq!(outcome.cached, vec!["beta".to_string()]);
    assert_eq!(*provider2.calls.lock().unwrap(), 1);
}

#[test]
fn cache_miss_on_output_io_error_emits_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let m = manifest_with_two_disjoint_generators();

    let host1 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![
            ("alpha.proto", simple_response("alpha.wado")),
            ("beta.graphql", simple_response("beta.wado")),
        ],
    );
    let provider1 = StubProvider::new(b"component-bytes".to_vec());
    run(&m, tmp.path(), &host1, &provider1);

    let cached_output = tmp.path().join("build/kiln/alpha/alpha.wado");
    assert!(cached_output.exists());
    std::fs::remove_file(&cached_output).unwrap();

    let host2 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![("alpha.proto", simple_response("alpha.wado"))],
    );
    let provider2 = StubProvider::new(b"component-bytes".to_vec());

    let outcome = run(&m, tmp.path(), &host2, &provider2);
    assert!(outcome.executed.contains(&"alpha".to_string()));

    let diags = host2.take_diagnostics();
    let saw_warning = diags.iter().any(|d| {
        matches!(d.severity, wado_compiler::Severity::Warning)
            && d.message.contains("kiln cache: failed to read")
            && d.message.contains("alpha.wado")
    });
    assert!(saw_warning, "expected warning diagnostic, got: {diags:?}");
}
