//! Integration tests for the Kiln pipeline driver.
//!
//! Exercises `wado_cli::kiln_driver::run_pipeline` through the crate's
//! public API, using a static in-memory generator provider and a mock
//! `CompilerHost`. All invocations originate inline; the manifest carries
//! only `[build-dependencies]`.

use std::path::Path;
use std::sync::Mutex;

use indexmap::IndexMap;
use wado_cli::kiln_driver::{GeneratorProvider, PipelineOutcome, ProviderError, run_pipeline};
use wado_cli::kiln_metadata;
use wado_compiler::compiler_host::{
    CompilerHost, Diagnostic, GeneratorOutputFile, GeneratorReadRecord, GeneratorRequest,
    GeneratorResponse, GeneratorRunnerError, SourceError,
};
use wado_compiler::kiln::{DeclSite, GeneratorModule, Invocation, InvocationPath};
use wado_manifest::Manifest;

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

fn empty_manifest() -> Manifest {
    "[package]\nname = \"host\"\nversion = \"0.1.0\""
        .parse()
        .unwrap()
}

fn run(
    manifest: &Manifest,
    root: &Path,
    host: &StubHost,
    provider: &StubProvider,
    inline: Vec<Invocation>,
) -> PipelineOutcome {
    runtime()
        .block_on(async { run_pipeline(manifest, root, host, provider, inline).await })
        .unwrap()
}

fn invocation(
    decl_file: &str,
    synthetic_id: &str,
    module: GeneratorModule,
    from: &str,
    output_dir: &str,
) -> Invocation {
    Invocation {
        decl_site: DeclSite {
            module: decl_file.to_string(),
            synthetic_id: synthetic_id.to_string(),
        },
        module,
        from: InvocationPath::normalize(from),
        inputs: vec![],
        output_dir: InvocationPath::normalize(output_dir),
        options_canonical: Vec::new(),
        raw_options: None,
    }
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

fn alpha_inv() -> Invocation {
    invocation(
        "entry.wado",
        "kiln-alpha",
        GeneratorModule::Spec("ns:proto@1.0.0".to_string()),
        "./alpha.proto",
        "build/kiln/kiln-alpha",
    )
}

fn beta_inv() -> Invocation {
    invocation(
        "entry.wado",
        "kiln-beta",
        GeneratorModule::Spec("ns:graphql@1.0.0".to_string()),
        "./beta.graphql",
        "build/kiln/kiln-beta",
    )
}

#[test]
fn end_to_end_two_generators_execute_cache_and_reuse() {
    let tmp = tempfile::tempdir().unwrap();
    let m = empty_manifest();
    let invs = vec![alpha_inv(), beta_inv()];

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

    let outcome = run(&m, tmp.path(), &host1, &provider1, invs.clone());
    assert_eq!(outcome.executed.len(), 2);
    assert!(outcome.executed.contains(&"kiln-alpha".to_string()));
    assert!(outcome.executed.contains(&"kiln-beta".to_string()));
    assert!(outcome.cached.is_empty());

    assert!(tmp.path().join("build/kiln/kiln-alpha/alpha.wado").exists());
    assert!(tmp.path().join("build/kiln/kiln-beta/beta.wado").exists());

    let alpha_meta = kiln_metadata::load(tmp.path(), "kiln-alpha")
        .unwrap()
        .expect("kiln-alpha metadata.json should be written after run");
    let beta_meta = kiln_metadata::load(tmp.path(), "kiln-beta")
        .unwrap()
        .expect("kiln-beta metadata.json should be written after run");
    assert_eq!(alpha_meta.invocation, "kiln-alpha");
    assert_eq!(beta_meta.invocation, "kiln-beta");

    // wado.lock must NOT be written by the kiln pipeline anymore (M9).
    assert!(
        !tmp.path().join("wado.lock").exists(),
        "kiln pipeline must no longer touch wado.lock"
    );

    let host2 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![],
    );
    let provider2 = StubProvider::new(b"component-bytes".to_vec());

    let outcome2 = run(&m, tmp.path(), &host2, &provider2, invs);
    assert_eq!(outcome2.cached.len(), 2);
    assert!(outcome2.executed.is_empty());
    assert_eq!(*provider2.calls.lock().unwrap(), 0);
}

#[test]
fn each_invocation_writes_its_own_metadata_file() {
    let tmp = tempfile::tempdir().unwrap();
    let m = empty_manifest();

    let z_inv = invocation(
        "entry.wado",
        "kiln-zebra",
        GeneratorModule::Spec("ns:zebra@1.0.0".to_string()),
        "./z.schema",
        "build/kiln/kiln-zebra",
    );
    let a_inv = invocation(
        "entry.wado",
        "kiln-alpha",
        GeneratorModule::Spec("ns:alpha@1.0.0".to_string()),
        "./a.schema",
        "build/kiln/kiln-alpha",
    );
    let m_inv = invocation(
        "entry.wado",
        "kiln-mango",
        GeneratorModule::Spec("ns:mango@1.0.0".to_string()),
        "./m.schema",
        "build/kiln/kiln-mango",
    );

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
    run(&m, tmp.path(), &host, &provider, vec![z_inv, a_inv, m_inv]);

    for id in ["kiln-zebra", "kiln-alpha", "kiln-mango"] {
        let m = kiln_metadata::load(tmp.path(), id)
            .unwrap()
            .unwrap_or_else(|| panic!("missing metadata.json for {id}"));
        assert_eq!(m.invocation, id);
    }
    assert!(
        !tmp.path().join("wado.lock").exists(),
        "kiln pipeline must no longer touch wado.lock"
    );
}

#[test]
fn end_to_end_input_change_invalidates_exactly_that_invocation() {
    let tmp = tempfile::tempdir().unwrap();
    let m = empty_manifest();
    let invs = vec![alpha_inv(), beta_inv()];

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
    run(&m, tmp.path(), &host1, &provider1, invs.clone());

    let host2 = StubHost::new(
        &[
            ("alpha.proto", b"alpha-schema-EDITED"),
            ("beta.graphql", b"beta-schema"),
        ],
        vec![("alpha.proto", simple_response("alpha.wado"))],
    );
    let provider2 = StubProvider::new(b"component-bytes".to_vec());

    let outcome = run(&m, tmp.path(), &host2, &provider2, invs);
    assert_eq!(outcome.executed, vec!["kiln-alpha".to_string()]);
    assert_eq!(outcome.cached, vec!["kiln-beta".to_string()]);
    assert_eq!(*provider2.calls.lock().unwrap(), 1);
}

#[test]
fn cache_miss_on_output_io_error_emits_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let m = empty_manifest();
    let invs = vec![alpha_inv(), beta_inv()];

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
    run(&m, tmp.path(), &host1, &provider1, invs.clone());

    let cached_output = tmp.path().join("build/kiln/kiln-alpha/alpha.wado");
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

    let outcome = run(&m, tmp.path(), &host2, &provider2, invs);
    assert!(outcome.executed.contains(&"kiln-alpha".to_string()));

    let diags = host2.take_diagnostics();
    let saw_warning = diags.iter().any(|d| {
        matches!(d.severity, wado_compiler::Severity::Warning)
            && d.message.contains("kiln cache: failed to read")
            && d.message.contains("alpha.wado")
    });
    assert!(saw_warning, "expected warning diagnostic, got: {diags:?}");
}

#[test]
fn pipeline_invocations_feed_compiler_options_for_use_redirect() {
    use wado_compiler::CompilerOptions;

    let tmp = tempfile::tempdir().unwrap();
    let m = empty_manifest();

    let inline_inv = Invocation {
        decl_site: DeclSite {
            module: "entry.wado".to_string(),
            synthetic_id: "kiln-abc12345".to_string(),
        },
        module: GeneratorModule::LocalPath(InvocationPath::normalize("../gen")),
        from: InvocationPath::normalize("./schema.proto"),
        inputs: vec![],
        output_dir: InvocationPath::normalize("build/kiln/kiln-abc12345"),
        options_canonical: Vec::new(),
        raw_options: None,
    };

    let generated_content = "pub fn greet() {}\n";
    let host = StubHost::new(
        &[("schema.proto", b"schema-content")],
        vec![(
            "schema.proto",
            GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "schema.wado".to_string(),
                    content: generated_content.to_string(),
                    is_entry: true,
                }],
                reads: Vec::<GeneratorReadRecord>::new(),
            },
        )],
    );
    let provider = StubProvider::new(b"component-bytes".to_vec());

    let outcome = runtime()
        .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider, vec![inline_inv]).await })
        .unwrap();

    assert!(!outcome.invocations.is_empty());

    let options = CompilerOptions {
        invocations: outcome.invocations,
        ..CompilerOptions::default()
    };
    assert!(!options.invocations.is_empty());
    let redirect = options
        .invocations
        .redirect("entry.wado", "./schema.proto")
        .expect("redirect must be populated for the inline decl_file + from pair");
    assert!(
        redirect.starts_with("kiln:"),
        "redirect URI must use the kiln: scheme, got `{redirect}`"
    );
    assert!(
        redirect.ends_with("/build/kiln/kiln-abc12345/schema.wado"),
        "redirect URI must point at the generated entry path, got `{redirect}`"
    );
}

#[test]
fn inline_invocation_populates_invocation_index_for_redirect() {
    let tmp = tempfile::tempdir().unwrap();
    let m = empty_manifest();

    let inline_inv = Invocation {
        decl_site: DeclSite {
            module: "entry.wado".to_string(),
            synthetic_id: "kiln-deadbeef".to_string(),
        },
        module: GeneratorModule::LocalPath(InvocationPath::normalize("../gen")),
        from: InvocationPath::normalize("./sample.proto"),
        inputs: vec![],
        output_dir: InvocationPath::normalize("build/kiln/kiln-deadbeef"),
        options_canonical: Vec::new(),
        raw_options: None,
    };

    let host = StubHost::new(
        &[("sample.proto", b"inline-schema")],
        vec![("sample.proto", simple_response("sample.wado"))],
    );
    let provider = StubProvider::new(b"component-bytes".to_vec());

    let outcome = runtime()
        .block_on(async { run_pipeline(&m, tmp.path(), &host, &provider, vec![inline_inv]).await })
        .unwrap();

    assert_eq!(outcome.executed.len(), 1, "inline should have executed");
    assert!(
        !outcome.invocations.is_empty(),
        "inline outcome must populate InvocationIndex"
    );
    let redirect = outcome
        .invocations
        .redirect("entry.wado", "./sample.proto")
        .expect("inline decl_file + from should redirect to the generated entry path");
    assert!(
        redirect.starts_with("kiln:"),
        "redirect URI must use the kiln: scheme, got `{redirect}`"
    );
    assert!(
        redirect.ends_with("/build/kiln/kiln-deadbeef/sample.wado"),
        "redirect URI must point at the generated entry path, got `{redirect}`"
    );
}
