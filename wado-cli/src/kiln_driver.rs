//! Kiln pipeline driver — manifest-lowering, plan computation, and
//! generator execution.
//!
//! This is the CLI-side glue between [`wado_manifest::Manifest`] and the
//! pure-data algorithmic module at [`wado_compiler::kiln`]. Its
//! responsibilities are:
//!
//! 1. Lower manifest invocations to the compiler's canonical form and
//!    dedup + topologically sort them via [`plan`].
//! 2. Execute a single invocation by calling
//!    [`wado_compiler::compiler_host::CompilerHost::run_generator`] and
//!    writing the response to disk under the invocation's output
//!    directory, with a canonical `#![generated]` header stamped on each
//!    file — see [`execute`].
//!
//! Cache-key composition, lockfile bookkeeping, and stale-output GC land
//! in later commits; this module is kept deliberately thin so each layer
//! can be reviewed in isolation.

use std::path::{Path, PathBuf};

use wado_compiler::compiler_host::{
    CompilerHost, GeneratorInputFile, GeneratorReadRecord, GeneratorRequest, GeneratorRunnerError,
    SourceError,
};
use wado_compiler::kiln::{
    DeclSite, GeneratedHeader, GeneratorModule, Invocation, InvocationPath, Plan, PlanError,
    build_plan, file_hash, generator_identity,
};
use wado_manifest::{GeneratorInvocation, GeneratorModuleRef, Manifest};

/// Output directory default when `[build.generators.<name>].output_dir` is
/// unset: `build/kiln/<name>`.
pub const DEFAULT_OUTPUT_DIR_PREFIX: &str = "build/kiln";

/// Outcome of [`plan`].
#[derive(Debug)]
pub struct PlanOutcome {
    /// Scheduled invocations in execution order. Empty when the manifest has
    /// no `[build.generators]` section.
    pub plan: Plan,
    /// The manifest root used to resolve relative paths, preserved so the
    /// caller can read input files without re-deriving it.
    pub manifest_root: PathBuf,
}

/// Errors from the driver. Reuses the compiler-side [`PlanError`] for
/// dedup/cycle diagnostics and adds CLI-specific variants for the
/// lowering step.
#[derive(Debug)]
pub enum DriverError {
    /// The manifest's `module = { ... }` inline form is not yet supported by
    /// the driver. M3c ships with `module = "ns:name@ver"` only; inline
    /// records land together with build-dependency resolution.
    UnsupportedInlineModule { invocation: String },
    /// Dedup / cycle error from the planner.
    Plan(PlanError),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::UnsupportedInlineModule { invocation } => write!(
                f,
                "[build.generators.{invocation}] uses an inline `module = {{ ... }}` record; \
                 only `module = \"ns:name@version\"` is supported in M3c"
            ),
            DriverError::Plan(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<PlanError> for DriverError {
    fn from(e: PlanError) -> Self {
        DriverError::Plan(e)
    }
}

/// Compute a plan for the manifest's `[build.generators]` section.
///
/// Takes an already-parsed manifest and the directory it was loaded from
/// (the `manifest_root`). Paths in `from` / `inputs` / `output_dir` are
/// interpreted relative to that root; the produced [`Invocation`]s carry
/// root-relative normalized paths.
///
/// Options are opaque to the driver at this stage: the canonical encoding
/// is produced by [`encode_options_canonical_provisional`] and passed
/// through as `options_canonical` bytes. M4 swaps the encoder without
/// changing this driver.
///
/// # Errors
///
/// - [`DriverError::UnsupportedInlineModule`] if any invocation uses inline
///   `module = { ... }`.
/// - [`DriverError::Plan`] for cycle / duplicate-generator errors.
///
/// The module-spec ↔ `[build-dependencies]` match is guaranteed by
/// `wado-manifest`'s `FromStr` validation, so this function trusts the
/// caller to pass a validated [`Manifest`].
pub fn plan(manifest: &Manifest, manifest_root: &Path) -> Result<PlanOutcome, DriverError> {
    let Some(build) = manifest.build.as_ref() else {
        return Ok(PlanOutcome {
            plan: Plan { order: Vec::new() },
            manifest_root: manifest_root.to_path_buf(),
        });
    };

    let mut invocations: Vec<Invocation> = Vec::with_capacity(build.generators.len());
    for (name, gi) in &build.generators {
        invocations.push(lower(name, gi)?);
    }

    let plan = build_plan(invocations)?;
    Ok(PlanOutcome {
        plan,
        manifest_root: manifest_root.to_path_buf(),
    })
}

/// Lower a single manifest-declared invocation to the compiler's canonical
/// form.
fn lower(name: &str, gi: &GeneratorInvocation) -> Result<Invocation, DriverError> {
    let module = match &gi.module {
        GeneratorModuleRef::Spec(spec) => GeneratorModule::Spec(spec.clone()),
        GeneratorModuleRef::Inline(_) => {
            return Err(DriverError::UnsupportedInlineModule {
                invocation: name.to_string(),
            });
        }
    };

    let from = InvocationPath::normalize(&gi.from);
    let inputs: Vec<InvocationPath> = gi
        .inputs
        .iter()
        .map(|p| InvocationPath::normalize(p))
        .collect();
    let output_dir = InvocationPath::normalize(
        gi.output_dir
            .as_deref()
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| format!("{DEFAULT_OUTPUT_DIR_PREFIX}/{name}"))
            .as_str(),
    );
    let options_canonical = encode_options_canonical_provisional(&gi.options);
    Ok(Invocation {
        decl_site: DeclSite::Manifest {
            name: name.to_string(),
        },
        module,
        from,
        inputs,
        output_dir,
        options_canonical,
    })
}

/// Provisional canonical TOML encoder — used by M3 until M4 replaces it with
/// the Component-Model lifted form.
///
/// Format (length-prefixed, depth-first, keys sorted alphabetically):
/// - `0` bool, 1 byte (0 / 1)
/// - `1` integer, 8 BE bytes
/// - `2` float, 8 BE bytes (IEEE-754 bits)
/// - `3` string, u64 BE length-prefix + UTF-8 bytes
/// - `4` array, u64 BE count + each element
/// - `5` table, u64 BE count + each (key string + value)
/// - `6` datetime, u64 BE length-prefix + RFC3339 string
#[must_use]
pub fn encode_options_canonical_provisional(value: &toml::Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_toml(&mut out, value);
    out
}

fn write_toml(out: &mut Vec<u8>, v: &toml::Value) {
    match v {
        toml::Value::Boolean(b) => {
            out.push(0);
            out.push(u8::from(*b));
        }
        toml::Value::Integer(i) => {
            out.push(1);
            out.extend_from_slice(&i.to_be_bytes());
        }
        toml::Value::Float(f) => {
            out.push(2);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        toml::Value::String(s) => {
            out.push(3);
            write_str(out, s);
        }
        toml::Value::Array(a) => {
            out.push(4);
            write_len(out, a.len());
            for item in a {
                write_toml(out, item);
            }
        }
        toml::Value::Table(t) => {
            out.push(5);
            let mut entries: Vec<(&String, &toml::Value)> = t.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            write_len(out, entries.len());
            for (k, val) in entries {
                write_str(out, k);
                write_toml(out, val);
            }
        }
        toml::Value::Datetime(dt) => {
            out.push(6);
            write_str(out, &dt.to_string());
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn write_len(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u64).to_be_bytes());
}

/// Outcome of [`execute`] for a single invocation.
///
/// Carries the information downstream layers (cache check, lockfile
/// writer, GC) need: every path that was written to disk, each written
/// file's content hash, and the list of `read-file` calls the generator
/// made during this run.
#[derive(Debug, Clone)]
pub struct InvocationRun {
    /// One entry per output file the generator produced. Paths are
    /// project-root-relative, normalized (`output_dir` joined with the
    /// generator-relative path and forward-slash-only).
    pub outputs: Vec<OutputHash>,
    /// Every `host::read-file` call the generator made, relayed verbatim
    /// from the runner. Fed into the next run's cache key so transitive
    /// reads invalidate the cache when their contents change.
    pub reads: Vec<GeneratorReadRecord>,
}

/// Output-file identity written to `wado.lock`.
///
/// Produced by [`execute`], consumed by the cache-check / lockfile layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputHash {
    /// Project-root-relative forward-slash path of the written file.
    pub path: String,
    /// SHA-256 of the file contents as written (header + generator body).
    pub hash: [u8; 32],
    /// Whether the generator marked this file as the invocation's entry
    /// module — the one a consuming `use ... from "<from>"` resolves to.
    pub is_entry: bool,
}

/// Errors from [`execute`].
#[derive(Debug)]
pub enum ExecuteError {
    /// Failed to load the primary schema or a declared input.
    LoadInput {
        path: String,
        source: SourceError,
    },
    /// The runner surfaced an error. `Unsupported` bubbles up here and is
    /// the signal a future consume-only layer (M7) watches for.
    Runner(GeneratorRunnerError),
    /// Filesystem I/O failure while writing a generated file or creating
    /// its parent directory.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A generator output's `path` is empty or attempts to escape the
    /// invocation's output directory (`..` segments, absolute paths).
    InvalidOutputPath { path: String },
}

impl std::fmt::Display for ExecuteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecuteError::LoadInput { path, source } => {
                write!(f, "kiln: failed to load input {path:?}: {source}")
            }
            ExecuteError::Runner(e) => write!(f, "kiln: {e}"),
            ExecuteError::Io { path, source } => {
                write!(f, "kiln: I/O error at {}: {source}", path.display())
            }
            ExecuteError::InvalidOutputPath { path } => {
                write!(f, "kiln: generator produced invalid output path {path:?}")
            }
        }
    }
}

impl std::error::Error for ExecuteError {}

/// Run one invocation: read its inputs, call the host's runner, write
/// the generator's outputs to disk with a `#![generated]` header.
///
/// `component_wasm` is the already-compiled generator component; the
/// caller (higher-level compile flow) is responsible for producing it.
/// `manifest_root` is the project-root directory; paths in `invocation`
/// are interpreted relative to it both for reading inputs and for
/// writing outputs.
///
/// The function does not consult or update `wado.lock`; caching is a
/// separate layer on top of this primitive.
///
/// # Errors
///
/// - [`ExecuteError::LoadInput`] if `host.load_source` fails for the
///   primary or any input.
/// - [`ExecuteError::Runner`] for any error the runner surfaces,
///   including `Unsupported` (which consume-only mode treats specially).
/// - [`ExecuteError::InvalidOutputPath`] if the generator returns a
///   path that escapes the output directory.
/// - [`ExecuteError::Io`] for filesystem failures.
pub async fn execute<H: CompilerHost>(
    invocation: &Invocation,
    component_wasm: &[u8],
    manifest_root: &Path,
    host: &H,
) -> Result<InvocationRun, ExecuteError> {
    let primary = load_input(host, &invocation.from).await?;
    let mut inputs = Vec::with_capacity(invocation.inputs.len());
    for p in &invocation.inputs {
        inputs.push(load_input(host, p).await?);
    }

    let request = GeneratorRequest {
        primary,
        inputs,
        options: invocation.options_canonical.clone(),
    };

    let response = host
        .run_generator(component_wasm, request)
        .await
        .map_err(ExecuteError::Runner)?;

    let output_dir_abs = manifest_root.join(invocation.output_dir.as_str());
    let by = generator_identity(&invocation.module);
    let mut sources: Vec<&InvocationPath> = Vec::with_capacity(1 + invocation.inputs.len());
    sources.push(&invocation.from);
    for p in &invocation.inputs {
        sources.push(p);
    }
    let source_paths: Vec<InvocationPath> = sources.iter().map(|p| (*p).clone()).collect();
    let header = GeneratedHeader::emit_with_paths(&by, &source_paths);

    let mut outputs = Vec::with_capacity(response.files.len());
    for file in &response.files {
        let rel = validate_rel_output_path(&file.path)?;
        let full_path = output_dir_abs.join(&rel);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ExecuteError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut bytes = Vec::with_capacity(header.len() + file.content.len());
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(file.content.as_bytes());
        std::fs::write(&full_path, &bytes).map_err(|source| ExecuteError::Io {
            path: full_path.clone(),
            source,
        })?;

        let normalized = InvocationPath::normalize(&format!(
            "{}/{}",
            invocation.output_dir.as_str(),
            rel.to_string_lossy()
        ));
        outputs.push(OutputHash {
            path: normalized.as_str().to_string(),
            hash: file_hash(&normalized, &bytes).hash,
            is_entry: file.is_entry,
        });
    }

    Ok(InvocationRun {
        outputs,
        reads: response.reads,
    })
}

async fn load_input<H: CompilerHost>(
    host: &H,
    path: &InvocationPath,
) -> Result<GeneratorInputFile, ExecuteError> {
    let content = host
        .load_source(path.as_str())
        .await
        .map_err(|source| ExecuteError::LoadInput {
            path: path.as_str().to_string(),
            source,
        })?;
    Ok(GeneratorInputFile {
        path: path.as_str().to_string(),
        content,
    })
}

/// Validate and convert a generator-supplied relative path into an
/// [`std::path::PathBuf`] suitable for joining onto the output dir.
///
/// Rejects empty paths, absolute paths, paths containing `..`, and paths
/// that start with `/`. Forward and backward slashes are accepted; the
/// result uses the host platform's separator via [`Path`].
fn validate_rel_output_path(p: &str) -> Result<PathBuf, ExecuteError> {
    if p.is_empty() {
        return Err(ExecuteError::InvalidOutputPath {
            path: p.to_string(),
        });
    }
    let candidate = Path::new(p);
    if candidate.is_absolute() {
        return Err(ExecuteError::InvalidOutputPath {
            path: p.to_string(),
        });
    }
    for comp in candidate.components() {
        use std::path::Component;
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ExecuteError::InvalidOutputPath {
                    path: p.to_string(),
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(candidate.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> (Manifest, PathBuf) {
        let m: Manifest = toml_str.parse().expect("valid manifest");
        (m, PathBuf::from("/tmp/does-not-matter"))
    }

    #[test]
    fn empty_manifest_produces_empty_plan() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert!(outcome.plan.order.is_empty());
    }

    #[test]
    fn manifest_without_build_section_is_empty_plan() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[dependencies]
"#,
        );
        assert!(plan(&m, &root).unwrap().plan.order.is_empty());
    }

    #[test]
    fn single_generator_lowered_with_defaults() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./schema.proto"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order.len(), 1);
        let inv = &outcome.plan.order[0];
        assert_eq!(inv.from.as_str(), "schema.proto");
        assert_eq!(inv.output_dir.as_str(), "build/kiln/proto");
        assert!(inv.inputs.is_empty());
        match &inv.module {
            GeneratorModule::Spec(s) => assert_eq!(s, "ns:proto@1.0.0"),
            other => panic!("expected Spec, got {other:?}"),
        }
    }

    #[test]
    fn explicit_output_dir_is_preserved() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./schema.proto"
output-dir = "gen/proto"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order[0].output_dir.as_str(), "gen/proto");
    }

    #[test]
    fn two_disjoint_generators_both_scheduled() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
proto = { package = "ns:proto", version = "^1.0.0" }
antlr = { package = "ns:antlr", version = "^1.0.0" }

[build.generators.proto]
module = "ns:proto@1.0.0"
from = "./a.proto"

[build.generators.grammar]
module = "ns:antlr@1.0.0"
from = "./a.g4"
"#,
        );
        let outcome = plan(&m, &root).unwrap();
        assert_eq!(outcome.plan.order.len(), 2);
    }

    #[test]
    fn inline_module_record_is_rejected() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[build.generators.proto]
module = { path = "../tools/proto" }
from = "./schema.proto"
"#,
        );
        let err = plan(&m, &root).unwrap_err();
        match err {
            DriverError::UnsupportedInlineModule { invocation } => {
                assert_eq!(invocation, "proto");
            }
            other => panic!("expected UnsupportedInlineModule, got {other:?}"),
        }
    }

    #[test]
    fn cycle_between_generators_surfaces_plan_error() {
        let (m, root) = parse(
            r#"
[package]
name = "a"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[build-dependencies]
gen-a = { package = "ns:a", version = "^1.0.0" }
gen-b = { package = "ns:b", version = "^1.0.0" }

[build.generators.first]
module = "ns:a@1.0.0"
from = "build/kiln/second/x.proto"
output-dir = "build/kiln/first"

[build.generators.second]
module = "ns:b@1.0.0"
from = "build/kiln/first/y.proto"
output-dir = "build/kiln/second"
"#,
        );
        let err = plan(&m, &root).unwrap_err();
        assert!(matches!(err, DriverError::Plan(PlanError::Cycle { .. })));
    }

    #[test]
    fn provisional_encoder_is_key_order_independent() {
        let a: toml::Value = toml::from_str("x = 1\ny = 2").unwrap();
        let b: toml::Value = toml::from_str("y = 2\nx = 1").unwrap();
        assert_eq!(
            encode_options_canonical_provisional(&a),
            encode_options_canonical_provisional(&b),
        );
    }

    #[test]
    fn provisional_encoder_distinguishes_string_from_int() {
        let a: toml::Value = toml::from_str("x = \"1\"").unwrap();
        let b: toml::Value = toml::from_str("x = 1").unwrap();
        assert_ne!(
            encode_options_canonical_provisional(&a),
            encode_options_canonical_provisional(&b),
        );
    }

    mod execute_tests {
        use super::*;
        use indexmap::IndexMap;
        use std::sync::Mutex;
        use wado_compiler::compiler_host::{
            Diagnostic, GeneratorOutputFile, GeneratorResponse, GeneratorRunnerError,
        };

        struct MockHost {
            sources: IndexMap<String, Vec<u8>>,
            response: Mutex<Option<Result<GeneratorResponse, GeneratorRunnerError>>>,
            requests: Mutex<Vec<GeneratorRequest>>,
        }

        impl MockHost {
            fn new(
                sources: &[(&str, &[u8])],
                response: Result<GeneratorResponse, GeneratorRunnerError>,
            ) -> Self {
                let sources = sources
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_vec()))
                    .collect();
                Self {
                    sources,
                    response: Mutex::new(Some(response)),
                    requests: Mutex::new(Vec::new()),
                }
            }
        }

        impl CompilerHost for MockHost {
            async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
                self.sources
                    .get(path)
                    .cloned()
                    .ok_or_else(|| SourceError::NotFound {
                        path: path.to_string(),
                    })
            }

            fn emit_diagnostic(&self, _d: Diagnostic) {}

            async fn run_generator(
                &self,
                _wasm: &[u8],
                request: GeneratorRequest,
            ) -> Result<GeneratorResponse, GeneratorRunnerError> {
                self.requests.lock().unwrap().push(request);
                self.response
                    .lock()
                    .unwrap()
                    .take()
                    .expect("mock response already consumed")
            }
        }

        fn sample_invocation() -> Invocation {
            Invocation {
                decl_site: DeclSite::Manifest {
                    name: "proto".to_string(),
                },
                module: GeneratorModule::Spec("ns:proto@1.0.0".to_string()),
                from: InvocationPath::normalize("schema.proto"),
                inputs: vec![InvocationPath::normalize("dep.proto")],
                output_dir: InvocationPath::normalize("build/kiln/proto"),
                options_canonical: vec![],
            }
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        }

        #[test]
        fn writes_outputs_with_header_and_reports_hashes() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![
                    GeneratorOutputFile {
                        path: "lib.wado".to_string(),
                        content: "pub fn hello() {}\n".to_string(),
                        is_entry: true,
                    },
                    GeneratorOutputFile {
                        path: "sub/mod.wado".to_string(),
                        content: "pub fn sub() {}\n".to_string(),
                        is_entry: false,
                    },
                ],
                reads: vec![],
            };
            let host = MockHost::new(
                &[
                    ("schema.proto", b"syntax = \"proto3\";"),
                    ("dep.proto", b"import \"other.proto\";"),
                ],
                Ok(response),
            );

            let inv = sample_invocation();
            let run = runtime()
                .block_on(async { execute(&inv, b"wasm-bytes", tmp.path(), &host).await })
                .unwrap();

            assert_eq!(run.outputs.len(), 2);
            assert_eq!(run.outputs[0].path, "build/kiln/proto/lib.wado");
            assert!(run.outputs[0].is_entry);
            assert_eq!(run.outputs[1].path, "build/kiln/proto/sub/mod.wado");
            assert!(!run.outputs[1].is_entry);

            let lib = std::fs::read_to_string(tmp.path().join("build/kiln/proto/lib.wado")).unwrap();
            assert!(lib.starts_with("#![generated(by = \"ns:proto@1.0.0\""));
            assert!(lib.contains("\"schema.proto\""));
            assert!(lib.contains("\"dep.proto\""));
            assert!(lib.ends_with("pub fn hello() {}\n"));

            let nested =
                std::fs::read_to_string(tmp.path().join("build/kiln/proto/sub/mod.wado")).unwrap();
            assert!(nested.starts_with("#![generated("));
        }

        #[test]
        fn unsupported_runner_error_bubbles_up() {
            let tmp = tempfile::tempdir().unwrap();
            let host = MockHost::new(
                &[
                    ("schema.proto", b"x"),
                    ("dep.proto", b"y"),
                ],
                Err(GeneratorRunnerError::Unsupported),
            );
            let err = runtime()
                .block_on(async {
                    execute(&sample_invocation(), b"wasm", tmp.path(), &host).await
                })
                .unwrap_err();
            assert!(matches!(
                err,
                ExecuteError::Runner(GeneratorRunnerError::Unsupported)
            ));
        }

        #[test]
        fn missing_primary_surfaces_load_input_error() {
            let tmp = tempfile::tempdir().unwrap();
            let host = MockHost::new(
                &[("dep.proto", b"y")],
                Err(GeneratorRunnerError::Unsupported),
            );
            let err = runtime()
                .block_on(async {
                    execute(&sample_invocation(), b"wasm", tmp.path(), &host).await
                })
                .unwrap_err();
            match err {
                ExecuteError::LoadInput { path, .. } => assert_eq!(path, "schema.proto"),
                other => panic!("expected LoadInput, got {other:?}"),
            }
        }

        #[test]
        fn escaping_output_path_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![GeneratorOutputFile {
                    path: "../escape.wado".to_string(),
                    content: "x".to_string(),
                    is_entry: true,
                }],
                reads: vec![],
            };
            let host = MockHost::new(
                &[
                    ("schema.proto", b"x"),
                    ("dep.proto", b"y"),
                ],
                Ok(response),
            );
            let err = runtime()
                .block_on(async {
                    execute(&sample_invocation(), b"wasm", tmp.path(), &host).await
                })
                .unwrap_err();
            match err {
                ExecuteError::InvalidOutputPath { path } => assert_eq!(path, "../escape.wado"),
                other => panic!("expected InvalidOutputPath, got {other:?}"),
            }
        }

        #[test]
        fn options_canonical_bytes_are_forwarded_to_request() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![],
                reads: vec![],
            };
            let host = MockHost::new(
                &[
                    ("schema.proto", b"x"),
                    ("dep.proto", b"y"),
                ],
                Ok(response),
            );
            let mut inv = sample_invocation();
            inv.options_canonical = vec![0xde, 0xad, 0xbe, 0xef];

            runtime()
                .block_on(async { execute(&inv, b"wasm", tmp.path(), &host).await })
                .unwrap();
            let reqs = host.requests.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].options, vec![0xde, 0xad, 0xbe, 0xef]);
            assert_eq!(reqs[0].primary.path, "schema.proto");
            assert_eq!(reqs[0].inputs.len(), 1);
            assert_eq!(reqs[0].inputs[0].path, "dep.proto");
        }

        #[test]
        fn reads_from_response_are_relayed_into_run() {
            let tmp = tempfile::tempdir().unwrap();
            let response = GeneratorResponse {
                files: vec![],
                reads: vec![GeneratorReadRecord {
                    path: "extra.proto".to_string(),
                    content_hash: [7u8; 32],
                }],
            };
            let host = MockHost::new(
                &[
                    ("schema.proto", b"x"),
                    ("dep.proto", b"y"),
                ],
                Ok(response),
            );
            let run = runtime()
                .block_on(async {
                    execute(&sample_invocation(), b"wasm", tmp.path(), &host).await
                })
                .unwrap();
            assert_eq!(run.reads.len(), 1);
            assert_eq!(run.reads[0].path, "extra.proto");
        }
    }
}
