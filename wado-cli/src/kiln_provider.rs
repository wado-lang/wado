//! CLI-side [`GeneratorProvider`] implementation.
//!
//! Resolves a [`GeneratorModule`] into a [`ResolvedGenerator`] (wasm
//! bytes + options descriptor + source-closure hash) for the Kiln
//! driver. It has two variants:
//!
//! - [`GeneratorModule::LocalPath`] — an inline relative-path `module:
//!   "./generator.wado"`, a directory `module: "../package-gale"` (rewritten to
//!   its `[world]."core:kiln/generator"` entry by
//!   [`crate::compile::rewrite_local_dir_modules`]), or a *path*
//!   `[build-dependencies]` specifier (rewritten by
//!   [`crate::compile::rewrite_build_dep_modules`]). All compile from source.
//! - [`GeneratorModule::Spec`] — a *registry* `[build-dependencies]` specifier
//!   (`module: "wado-lang:gale"` / `module: "lib:gale"`), or, in single-file mode
//!   with no `wado.toml`, the same specifier with its source (`version` +
//!   `registry`) supplied inline at the `use` site. Its published version is
//!   pulled from the `core-kiln-generator` world sub-path as a prebuilt component
//!   and its options descriptor is recovered from the component WIT (see
//!   [`resolve_spec`] and [`crate::kiln_wit`]). A spec with neither a matching
//!   build-dependency nor an inline registry surfaces [`ProviderError::Unsupported`].
//!
//! For `LocalPath`, `resolve` consults the cache (gated by `no_cache`) and
//! falls back to a fresh [`wado_compiler::compile_with_options`] run on miss,
//! always rewriting the cache so a subsequent run sees a warm tree.
//!
//! A generator's identity is the hash of its source closure, and its component
//! is stored under that hash, so a changed generator writes a new name rather
//! than overwriting one that no longer describes it. [`INDEX_FILE`] records
//! what the project last built, which is how a later run finds the component
//! without recompiling to learn its hash.
//!
//! The toolchain is not in that hash, so an upgraded compiler reuses what the
//! previous one built until `build/` is cleared. Adding it is not available:
//! the same hash is the generator's identity in every consumer's committed
//! `<primary>.kiln.json`.
//!
//! The driver is responsible for not calling `resolve` more than once
//! per unique module per pipeline run — this provider holds no
//! in-memory artifact state.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use indexmap::IndexMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use wado_compiler::kiln::{GeneratorModule, InvocationPath, OptionsDescriptor};
use wado_compiler::lexer::lex;
use wado_compiler::token::canonical_token_bytes;
use wado_compiler::{CompilerHost, CompilerOptions, Diagnostic, LogLevel};

use crate::compiler_host::FilesystemCompilerHost;
use crate::kiln_driver::{GeneratorProvider, ProviderError, ResolvedGenerator};
use crate::oci;

/// Compiled generator components, each named by the hash of its sources.
/// Gitignored. A registry (`Spec`) generator is prebuilt and caches in the
/// shared `~/wado/` tree (see [`crate::cache`]) instead.
pub const CACHE_DIR: &str = "build/kiln/generators";

/// See [`GeneratorIndex`]. Gitignored.
pub const INDEX_FILE: &str = "build/kiln/generators.json";

/// One in-flight build per generator. A process global because `wado test`
/// builds a provider per fixture, so the lock must outlive any one of them;
/// async because it is held across the build.
///
/// Not a file lock: the component is content-addressed, so racing processes
/// write identical bytes and only duplicate work.
static BUILDS: LazyLock<Mutex<IndexMap<(PathBuf, String), Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(IndexMap::new()));

/// Serializes read-modify-write of a project index; held only across the JSON
/// round-trip. Process-local, so racing processes can still drop an entry —
/// costing a rebuild, not correctness.
static INDEX_WRITES: Mutex<()> = Mutex::new(());

/// The build lock for one generator. The map guard is released before the
/// caller ever awaits, so registering a new generator never blocks a build.
fn build_lock(index_path: &Path, module_path: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut builds = BUILDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (index_path.to_path_buf(), module_path.to_string());
    Arc::clone(builds.entry(key).or_default())
}

#[derive(Debug, Clone, Default)]
pub struct RegistryContext {
    /// The project's `[build-dependencies]`, keyed as declared (the
    /// `ns:name` coordinate a `module: "ns:name"` spec resolves against).
    pub build_dependencies: indexmap::IndexMap<String, wado_manifest::Dependency>,
    /// The project's `[registries]` (alias → `oci://…` URL).
    pub registries: indexmap::IndexMap<String, String>,
    /// Locked generator versions from `wado.lock` (`coordinate → version`).
    /// When present, a spec resolves to the pinned version without a registry
    /// version listing.
    pub locked_versions: indexmap::IndexMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct CliGeneratorProvider {
    manifest_root: PathBuf,
    compile_count: Arc<AtomicUsize>,
    /// When true, skip reads from the on-disk generator cache. Writes
    /// still happen so the next non-bypass run sees a warm tree.
    no_cache: bool,
    /// `[build-dependencies]` + `[registries]` used to resolve a
    /// [`GeneratorModule::Spec`] (`module: "ns:name"`) against the
    /// registry. Empty for callers that only use local generators.
    registry: RegistryContext,
}

impl CliGeneratorProvider {
    #[must_use]
    pub fn new(manifest_root: PathBuf) -> Self {
        Self {
            manifest_root,
            compile_count: Arc::new(AtomicUsize::new(0)),
            no_cache: false,
            registry: RegistryContext::default(),
        }
    }

    #[must_use]
    pub fn with_no_cache(mut self, no_cache: bool) -> Self {
        self.no_cache = no_cache;
        self
    }

    /// Attach the `[build-dependencies]` + `[registries]` a
    /// [`GeneratorModule::Spec`] resolves against.
    #[must_use]
    pub fn with_registry_context(mut self, registry: RegistryContext) -> Self {
        self.registry = registry;
        self
    }

    /// Number of times the inner wado-compiler pipeline has actually
    /// run (cache hits do not contribute). Shared across `Clone`s of
    /// the provider so test code with multiple handles still observes
    /// a single total.
    #[must_use]
    pub fn compile_count(&self) -> usize {
        self.compile_count.load(Ordering::SeqCst)
    }

    /// The generator's real, absolute path.
    ///
    /// The compiler embeds this string in the component it produces, while the
    /// cache names that component after its *sources* — so it has to be one
    /// canonical spelling per file, or the same generator compiles to different
    /// bytes depending on which project addressed it or which directory the
    /// command ran from, and both land under a single name.
    ///
    /// `canonicalize` rather than lexical folding, because a package reached
    /// through a symlink resolves its `../` differently than text does: folding
    /// `link/../shared-gen` textually names a directory that may not exist.
    /// It needs the file to be there, which is the normal case; when it is not,
    /// fall back to the lexical form so the caller's not-found error names the
    /// path the user wrote rather than an io error.
    fn resolve_path(&self, rel: &str) -> PathBuf {
        let joined = self.manifest_root.join(Path::new(rel));
        std::fs::canonicalize(&joined).unwrap_or_else(|_| normalize_path(&joined))
    }

    fn index_path(&self) -> PathBuf {
        self.manifest_root.join(INDEX_FILE)
    }

    /// Where the component built from `source_hash` is cached.
    fn component_path(&self, source_hash: &str) -> PathBuf {
        self.manifest_root
            .join(CACHE_DIR)
            .join(format!("{source_hash}.wasm"))
    }

    /// The indexed identity of the generator at `module_path`, while the
    /// sources it records still hash the same. `None` otherwise — the caller
    /// rebuilds and rewrites.
    ///
    /// The hash is recomputed from the validated entries rather than read back,
    /// so a hand-edited index cannot pin an identity its own source list
    /// contradicts.
    fn indexed_identity(&self, module_path: &str, base: &Path) -> Option<IndexedGenerator> {
        let index = GeneratorIndex::load(&self.index_path())?;
        let recorded = index.generators.get(module_path)?;
        let mut validated: Vec<(String, [u8; 32])> = Vec::with_capacity(recorded.sources.len());
        for entry in &recorded.sources {
            let bytes = std::fs::read(base.join(&entry.path)).ok()?;
            let actual = hash_source(&entry.path, &bytes);
            if actual != hex32_to_array(&entry.hash)? {
                return None;
            }
            validated.push((entry.path.clone(), actual));
        }
        (combined_sources_hash(&validated) == recorded.source_hash).then(|| recorded.clone())
    }

    /// Record `identity` for `module_path`, merging with the project's other
    /// generators. Best-effort: a refusal costs the next run a rebuild.
    fn write_index(&self, module_path: &str, identity: &IndexedGenerator) {
        let path = self.index_path();
        let _writing = INDEX_WRITES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut index = GeneratorIndex::load(&path).unwrap_or_default();
        index.version = INDEX_VERSION;
        let superseded = index
            .generators
            .insert(module_path.to_string(), identity.clone())
            .map(|previous| previous.source_hash);
        index.generators.sort_keys();
        if let Ok(mut bytes) = serde_json::to_vec_pretty(&index) {
            bytes.push(b'\n');
            let _ = crate::cache::write_atomic(&path, &bytes);
        }
        // Content-addressed names are never reused, so nothing else would ever
        // reclaim this. Two modules with identical sources share a component,
        // hence the reference check.
        if let Some(orphan) = superseded.filter(|hash| {
            hash != &identity.source_hash
                && !index.generators.values().any(|g| &g.source_hash == hash)
        }) {
            let _ = std::fs::remove_file(self.component_path(&orphan));
        }
    }

    fn read_local_source(
        &self,
        path: &wado_compiler::kiln::InvocationPath,
    ) -> Result<(PathBuf, Vec<u8>, String), ProviderError> {
        let abs = self.resolve_path(path.as_str());
        if !abs.exists() {
            return Err(ProviderError::Internal {
                message: format!(
                    "kiln: generator path `{}` does not exist (relative to manifest root {})",
                    path.as_str(),
                    self.manifest_root.display(),
                ),
            });
        }
        // A directory module is resolved to its package's
        // `[world]."core:kiln/generator"` entry up front by
        // `compile::rewrite_local_dir_modules`, so a directory reaching the
        // provider is a package that declared no such entry. Report that
        // instead of letting `std::fs::read` surface a bare "Is a directory".
        if abs.is_dir() {
            return Err(ProviderError::Internal {
                message: format!(
                    "kiln: generator module `{}` is a directory but is not a generator package: \
                     its `wado.toml` is missing or declares no [world].\"core:kiln/generator\" \
                     entry. Point `module` at a generator package directory or a `.wado` file",
                    path.as_str(),
                ),
            });
        }
        let source = std::fs::read(&abs).map_err(|e| ProviderError::Internal {
            message: format!(
                "kiln: failed to read generator source at `{}`: {e}",
                abs.display()
            ),
        })?;
        let source_str = match std::str::from_utf8(&source) {
            Ok(s) => s.to_string(),
            Err(_) => {
                return Err(ProviderError::Internal {
                    message: format!(
                        "kiln: generator source at `{}` is not valid UTF-8",
                        abs.display()
                    ),
                });
            }
        };
        Ok((abs, source, source_str))
    }

    /// Compile the generator, publish the component under its source hash, and
    /// record that identity in the project index.
    async fn compile_local(
        &self,
        path_str: String,
        abs: PathBuf,
        source_str: String,
    ) -> Result<CompileArtifacts, ProviderError> {
        self.compile_count.fetch_add(1, Ordering::SeqCst);

        let base_path = abs.parent().map(Path::to_path_buf).unwrap_or_default();
        let abs_str = abs.to_string_lossy().to_string();
        // The compiler embeds this, and the source closure records the entry
        // by its bare name — so anything else would let two copies of one
        // generator compile to different bytes under a single name. Imports
        // resolve against the host's base, not this string.
        let embedded_name = abs
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| abs_str.clone());
        let path_str_for_index = path_str.clone();
        let entry_name = embedded_name.clone();
        let entry_bytes = source_str.clone().into_bytes();
        let recording_base = base_path.clone();
        let loaded = Arc::new(Mutex::new(Vec::<(String, [u8; 32])>::new()));
        let loaded_for_task = loaded.clone();

        // `compile_with_options` captures a `Logger<H>` whose internal
        // `Cell<usize>` makes the returned future `!Send`. Run the whole
        // async compile on a blocking thread with its own current-thread
        // runtime so the multi-thread driver runtime only sees a `Send`-
        // able `spawn_blocking` future.
        let artifacts: Result<CompileArtifacts, ProviderError> =
            tokio::task::spawn_blocking(move || {
                let host = SilentHost {
                    inner: FilesystemCompilerHost::with_log_level(base_path, LogLevel::Warn),
                    loaded: loaded_for_task,
                };
                let options = CompilerOptions {
                    // Optimize the generator: its runtime dominates for heavy
                    // grammars (minutes of generation), and CI runs it cold on
                    // every build since build/kiln is gitignored, so the O2
                    // compile cost is repaid many times over by faster generation.
                    opt_level: wado_compiler::OptLevel::O2,
                    target_world: Some("core:kiln/generator".to_string()),
                    skip_validation: false,
                    log_level: Some(LogLevel::Warn),
                    ..CompilerOptions::default()
                };
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| ProviderError::Internal {
                        message: format!(
                            "kiln: failed to start inner runtime for generator compile: {e}"
                        ),
                    })?;
                let result = rt.block_on(async {
                    wado_compiler::compile_with_options(
                        &source_str,
                        &host,
                        Some(&embedded_name),
                        options,
                    )
                    .await
                });
                match result {
                    Ok(r) => Ok(CompileArtifacts {
                        wasm: r.wasm,
                        descriptor: r.kiln_options_descriptor,
                        // Filled in by `compile_local` after the recording
                        // host's `loaded` queue is drained.
                        source_hash: String::new(),
                    }),
                    Err(_) => Err(ProviderError::Internal {
                        message: format!(
                            "kiln: failed to compile generator `{path_str}` to component"
                        ),
                    }),
                }
            })
            .await
            .map_err(|e| ProviderError::Internal {
                message: format!("kiln: generator compile task panicked or was cancelled: {e}"),
            })?;

        let mut artifacts = artifacts?;

        // Drain the recorded sources, dedup paths (the compiler can request
        // the same module more than once). The combined hash is what the kiln
        // driver records in `Metadata::generator_source_hash`.
        let mut recorded = loaded
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        // The entry never goes through `load_source`, so the recording host
        // never sees it; without seeding it, editing it would not move the
        // hash.
        recorded.push((entry_name.clone(), hash_source(&entry_name, &entry_bytes)));
        let sources = dedup_sort_sources(make_relative_sources(&recording_base, recorded));
        let combined_hash = combined_sources_hash(&sources);
        artifacts.source_hash.clone_from(&combined_hash);

        // Best-effort: the caller holds the fresh bytes, so a refusal costs
        // the next run a rebuild.
        let _ = crate::cache::write_atomic(&self.component_path(&combined_hash), &artifacts.wasm);
        self.write_index(
            &path_str_for_index,
            &IndexedGenerator {
                source_hash: combined_hash,
                descriptor: artifacts.descriptor.clone(),
                sources: sources
                    .iter()
                    .map(|(path, hash)| SourceEntry {
                        path: path.clone(),
                        hash: hex32(hash),
                    })
                    .collect(),
            },
        );

        Ok(artifacts)
    }
}

/// Re-anchor the recorded paths at `base`, walking up with `..` for anything
/// outside it.
///
/// Each has to resolve back to the file it names, since it is re-hashed on
/// every read, and it feeds the identity hash, so it must carry nothing
/// machine-specific. One that cannot be expressed relative to `base` is kept
/// verbatim: its hash still pins content, so it simply misses elsewhere.
fn make_relative_sources(base: &Path, raw: Vec<(String, [u8; 32])>) -> Vec<(String, [u8; 32])> {
    let base = normalize_path(base);
    raw.into_iter()
        .map(|(path, hash)| {
            // `join` replaces on an absolute path, so this lands both kinds
            // on the same footing. Normalized first: diffing components that
            // still carry `.`/`..` yields nonsense like `.././foo.wado`, which
            // resolves to nothing and quietly makes every later run a miss.
            let target = normalize_path(&base.join(&path));
            (relative_to(&base, &target).unwrap_or(path), hash)
        })
        .collect()
}

/// Fold `.` and `..` out of `path` without touching the filesystem. A `..` at
/// the root, or leading a relative path, has nothing to pop and is kept.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // `PathBuf::pop` would remove a `..` this loop just pushed,
                // turning `../../pkg` into `pkg`. Above the root, `/..` is `/`.
                match out.components().next_back() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    Some(Component::RootDir | Component::Prefix(_)) => {}
                    Some(Component::CurDir | Component::ParentDir) | None => {
                        out.push(component);
                    }
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component);
            }
        }
    }
    out
}

/// `target` expressed relative to `base`, with `..` for each level `target`
/// sits above it. Both must already be normalized. `None` when the two share no
/// root to walk between.
fn relative_to(base: &Path, target: &Path) -> Option<String> {
    use std::path::Component;

    if base.is_absolute() != target.is_absolute() {
        return None;
    }
    let mut base_parts = base.components().peekable();
    let mut target_parts = target.components().peekable();
    while base_parts.peek().is_some() && base_parts.peek() == target_parts.peek() {
        base_parts.next();
        target_parts.next();
    }
    let mut out = String::new();
    for _ in base_parts.filter(|c| !matches!(c, Component::CurDir)) {
        out.push_str("../");
    }
    // Always `/`-separated: the spelling feeds a hash.
    let mut rest = target_parts.peekable();
    while let Some(part) = rest.next() {
        out.push_str(&part.as_os_str().to_string_lossy());
        if rest.peek().is_some() {
            out.push('/');
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Provider-internal twin of [`ResolvedGenerator`]. Kept distinct so
/// `compile_local` can fill `source_hash` after the inner pipeline
/// returns (the recording host's `loaded` queue isn't drained until
/// then), without exposing a half-built value to the trait surface.
struct CompileArtifacts {
    wasm: Vec<u8>,
    descriptor: Option<OptionsDescriptor>,
    source_hash: String,
}

/// Generator-component ABI generation. The compiled component embeds the
/// `core:kiln` world types, so bump this on any `core:kiln/generator.wit` /
/// `emit_kiln_world_types` change to invalidate caches built against the old
/// ABI even when the generator source is unchanged. `v2`: `raw-request.options`
/// became `list<u8>` (CBOR). `v3`: the world is import-only,
/// `raw-request` is gone, and `generate` takes typed `(primary, inputs,
/// options)` parameters.
const KILN_GENERATOR_ABI_TAG: &[u8] = b"kiln-generator-v3\n";

/// `CompilerHost` wrapper used by `compile_local` for two reasons:
///
/// 1. Swallow generator-compile diagnostics (info/debug) so the
///    consuming project's stderr isn't flooded with nested compile
///    output; the provider surfaces a single `ProviderError` instead.
/// 2. Record every `load_source` call into `loaded` (path + content
///    hash) so the transitive *load closure* — `.wado` modules **and**
///    raw assets pulled via `#include_bytes` — is persisted into the
///    index, letting an edit to either kind invalidate the cache. The recording is best-effort: duplicate paths from the
///    compiler are collapsed by the dedup step in `compile_local`.
struct SilentHost {
    inner: FilesystemCompilerHost,
    loaded: Arc<Mutex<Vec<(String, [u8; 32])>>>,
}

impl CompilerHost for SilentHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, wado_compiler::SourceError>> + Send {
        let path_owned = path.to_string();
        let loaded = self.loaded.clone();
        let inner_fut = self.inner.load_source(path);
        async move {
            let bytes = inner_fut.await?;
            let hash = hash_source(&path_owned, &bytes);
            if let Ok(mut guard) = loaded.lock() {
                guard.push((path_owned, hash));
            }
            Ok(bytes)
        }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        // Only surface actual errors; silence info/debug logs so the
        // consuming project's stderr isn't flooded with nested compile
        // output.
        if matches!(
            diagnostic.severity,
            wado_compiler::Severity::Error | wado_compiler::Severity::Warning
        ) {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "  [kiln generator compile] {diagnostic}");
        }
    }

    // Everything this host does not decorate must reach the inner one: the
    // trait defaults would hand the generator an empty `[dependencies]` index
    // and no environment.
    fn dependency_index(&self) -> wado_compiler::DependencyIndex {
        self.inner.dependency_index()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.inner.env_var(name)
    }
}

/// Build a [`ResolvedGenerator`] from a prebuilt generator component: recover
/// the options descriptor from its WIT and hash the bytes as the source hash.
fn resolved_generator_from_wasm(wasm: Vec<u8>) -> Result<ResolvedGenerator, String> {
    let descriptor = crate::kiln_wit::options_descriptor_from_component(&wasm)?;
    let source_hash = hex32(&sha256_of(&wasm));
    Ok(ResolvedGenerator {
        wasm,
        descriptor,
        source_hash,
    })
}

fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// `true` when `path` looks like a Wado source file. Other extensions
/// (binary blobs from `#include_bytes`, text payloads from
/// `#include_str`, raw `.wat` assets) keep the byte-content hash so a
/// single-byte edit still invalidates the cache.
fn is_wado_source(path: &str) -> bool {
    matches!(path.rsplit_once('.'), Some((_, ext)) if ext.eq_ignore_ascii_case("wado"))
}

/// Source-file hash routed by extension. `.wado` files run through the
/// canonical token-stream encoding so comments, doc comments, and
/// formatting changes do not perturb the hash; everything else falls
/// back to a plain content hash.
///
/// On lex failure we deliberately fall back to the byte hash: the file
/// might be `.wado` shaped but not actually parseable
/// (broken on disk between the cache write and validate), and the
/// downstream cache check will still detect drift via byte-hash
/// inequality.
fn hash_source(path: &str, bytes: &[u8]) -> [u8; 32] {
    if !is_wado_source(path) {
        return sha256_of(bytes);
    }
    let Ok(source) = std::str::from_utf8(bytes) else {
        return sha256_of(bytes);
    };
    let lex_result = lex(source);
    // If the lexer recovered any error the source is malformed; fall back to
    // the byte hash so we don't bake a half-tokenised stream into the cache.
    if !lex_result.errors.is_empty() {
        return sha256_of(bytes);
    }
    // Canonical token bytes ignore spans and (because the lexer peels
    // comments off into a side channel before returning the token list)
    // every line/block/doc comment.
    let mut buf: Vec<u8> = Vec::with_capacity(bytes.len());
    buf.extend_from_slice(b"wado-token-stream-v1\n");
    for tok in &lex_result.tokens {
        canonical_token_bytes(&mut buf, &tok.kind);
    }
    // Shebang and the `__DATA__` trailer carry semantic content that
    // the parser still sees, so fold them into the hash too.
    if let Some(shebang) = &lex_result.shebang {
        buf.push(b'#');
        buf.extend_from_slice(shebang.as_bytes());
        buf.push(0);
    }
    if let Some(data) = &lex_result.data_section {
        buf.push(b'D');
        buf.extend_from_slice(data.as_bytes());
        buf.push(0);
    }
    sha256_of(&buf)
}

/// What a project knows about the generators it has built, so it can find
/// their components again without recompiling to learn their hashes.
///
/// A lookup aid, nothing more: gitignored, and deleting it costs a rebuild. It
/// makes no claim about the *generated outputs* — that is
/// `<primary>.kiln.json`'s job, per invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GeneratorIndex {
    /// Schema version. A mismatch is a cache miss, so a bump needs no
    /// migration: the next build rewrites the file.
    version: u32,
    /// Keyed by the generator's manifest-root-relative module path.
    generators: IndexMap<String, IndexedGenerator>,
}

impl GeneratorIndex {
    /// `None` when the file is absent, unreadable, malformed, or a version this
    /// build does not speak — all of which mean "rebuild and rewrite".
    fn load(path: &Path) -> Option<Self> {
        let index: Self = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        (index.version == INDEX_VERSION).then_some(index)
    }
}

/// One generator's last known build.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexedGenerator {
    /// Hash of `sources`; names the component, and is the identity the driver
    /// records in `Metadata::generator_source_hash`.
    source_hash: String,
    /// `null` for a generator with no `pub struct Options`.
    descriptor: Option<OptionsDescriptor>,
    /// Every file the compile loaded — `.wado` modules and assets routed
    /// through `CompilerHost::load_source`; stdlib bypasses the host and never
    /// appears. Re-hashed on read, so an edit to a transitive import
    /// invalidates the entry.
    sources: Vec<SourceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceEntry {
    path: String,
    /// Hex-encoded SHA-256 of the file's bytes at compile time.
    hash: String,
}

/// Moves with the `combined_sources_hash` magic, so a downgrade is a miss
/// rather than a silent mix of hash generations.
const INDEX_VERSION: u32 = 3;

fn dedup_sort_sources(mut sources: Vec<(String, [u8; 32])>) -> Vec<(String, [u8; 32])> {
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources.dedup_by(|a, b| a.0 == b.0);
    sources
}

fn combined_sources_hash(sources: &[(String, [u8; 32])]) -> String {
    let mut hasher = Sha256::new();
    // v3: `.wado` sources hash as token streams (see `hash_source`), and the
    // ABI generation folds in here because this is a generator's only
    // identity. The magic tracks `INDEX_VERSION`.
    hasher.update(b"kiln-generator-sources-v3\n");
    hasher.update(KILN_GENERATOR_ABI_TAG);
    for (path, hash) in sources {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
        hasher.update(hash);
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hex32_to_array(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The OCI world segment a Kiln generator publishes to (`wado publish` maps
/// the `core:kiln/generator` world to this repository sub-path).
impl CliGeneratorProvider {
    /// Resolve a `module: "ns:name"` / `module: "lib:nick"` registry generator:
    /// look the specifier up in `[build-dependencies]`, pick the version (the
    /// `wado.lock` pin when one exists, else the highest published version
    /// matching the requirement), pull its `core-kiln-generator` component, and
    /// recover its options descriptor from the component WIT.
    ///
    /// A path build-dependency is rewritten to `LocalPath` before it reaches the
    /// provider, so a `Spec` here is always a registry entry.
    ///
    /// The pulled component is cached under `build/kiln/generators/` keyed by the
    /// resolved coordinate — the same location `wado fetch` pre-populates — so a
    /// warm cache (or a fetched tree) short-circuits the registry round-trip. A
    /// published version is immutable, so this is sound. `--no-cache` forces a
    /// re-pull.
    async fn resolve_spec(
        &self,
        spec: &wado_compiler::kiln::GeneratorSpec,
    ) -> Result<ResolvedGenerator, ProviderError> {
        let parts = crate::build_dep::parse_spec(&spec.spec);
        if parts.submodule.is_some() {
            return Err(ProviderError::Unsupported {
                message: format!(
                    "kiln: submodule paths in a generator specifier are not yet supported: `{}`",
                    spec.spec
                ),
            });
        }

        // The specifier resolves against a `[build-dependencies]` entry when the
        // manifest declares it; otherwise its source is supplied inline at the
        // `use` site (single-file mode).
        let (registry_url, package, version) =
            if let Some(dep) = self.registry.build_dependencies.get(parts.key) {
                if spec.version.is_some() || spec.registry.is_some() {
                    return Err(ProviderError::Unsupported {
                        message: format!(
                            "kiln: inline `version`/`registry` are not allowed for `{}`, which is \
                         declared in [build-dependencies] (manifest and inline are exclusive)",
                            parts.key
                        ),
                    });
                }
                let wado_manifest::DependencySource::Registry {
                    registry,
                    package,
                    version,
                } = &dep.source
                else {
                    return Err(ProviderError::Unsupported {
                        message: format!(
                            "kiln: generator `{}` must resolve to a registry or path \
                         [build-dependencies] entry; git and workspace sources are not supported",
                            spec.spec
                        ),
                    });
                };
                let url = self.registry_url(registry.as_deref())?;
                let ver = self
                    .resolve_version(&url, package, parts.version, Some(version.as_str()))
                    .await?;
                (url, package.clone(), ver)
            } else {
                // Single-file / inline mode: the coordinate is the key, and the
                // registry + version come from the inline `generator` fields.
                let package = parts.key.to_string();
                let url = spec
                    .registry
                    .clone()
                    .or_else(|| self.registry.registries.get("default").cloned())
                    .ok_or_else(|| ProviderError::Unsupported {
                        message: format!(
                            "kiln: generator `{}` is not in [build-dependencies]; supply an inline \
                         `registry: \"oci://…\"` (single-file) or declare it in a wado.toml",
                            spec.spec
                        ),
                    })?;
                let ver = self
                    .resolve_version(&url, &package, parts.version, spec.version.as_deref())
                    .await?;
                (url, package, ver)
            };

        // Identity is the *resolved* coordinate + version: the shared-cache path
        // keys on both, so a version bump misses and a coordinate and a `lib:`
        // nickname for the same package share one entry (WEP "generator identity").
        let package = &package;
        if !self.no_cache
            && let Some(resolved) = self.try_read_spec_cache(&registry_url, package, &version)
        {
            return Ok(resolved);
        }

        let reference = oci::world_reference(
            &registry_url,
            package,
            crate::build_dep::GENERATOR_WORLD_SEGMENT,
            &version,
        )
        .map_err(|message| ProviderError::Internal { message })?;
        let wasm = oci::pull_component(&reference)
            .await
            .map_err(|e| ProviderError::Internal {
                message: format!("kiln: fetching generator {reference}: {e}"),
            })?;
        let resolved =
            resolved_generator_from_wasm(wasm.to_vec()).map_err(|e| ProviderError::Internal {
                message: format!("kiln: reading options of generator {reference}: {e}"),
            })?;
        self.write_spec_cache(&registry_url, package, &version, &resolved.wasm);
        Ok(resolved)
    }

    /// Pick the concrete version: an explicit `@version` pin, then the
    /// `wado.lock` pin, then a live registry listing against the requirement.
    /// Errors when no source of a version is available.
    async fn resolve_version(
        &self,
        registry_url: &str,
        package: &str,
        pin: Option<&str>,
        req: Option<&str>,
    ) -> Result<String, ProviderError> {
        if let Some(p) = pin {
            return Ok(p.to_string());
        }
        if let Some(v) = self.registry.locked_versions.get(package) {
            return Ok(v.clone());
        }
        let Some(req) = req else {
            return Err(ProviderError::Unsupported {
                message: format!(
                    "kiln: generator `{package}` has no version; add `@version` to the module, \
                     an inline `version`, or a [build-dependencies] entry"
                ),
            });
        };
        crate::build_dep::resolve_generator_version(registry_url, package, req)
            .await
            .map_err(|message| ProviderError::Unsupported { message })
    }

    /// Resolve a registry alias (`None` → `default`) to its `oci://…` URL.
    fn registry_url(&self, alias: Option<&str>) -> Result<String, ProviderError> {
        let alias = alias.unwrap_or("default");
        self.registry
            .registries
            .get(alias)
            .cloned()
            .ok_or_else(|| ProviderError::Unsupported {
                message: format!("kiln: no `[registries].{alias}` to fetch the generator from"),
            })
    }

    /// Read a cached (or `wado fetch`-populated) generator component from the
    /// shared `~/wado/` cache and rebuild its `ResolvedGenerator`. The descriptor
    /// is recovered from the component WIT every time rather than cached
    /// separately, so a component written by `wado fetch` (which does not persist
    /// no descriptor) is a full cache hit. `None` on a cache miss or an
    /// unreadable component.
    fn try_read_spec_cache(
        &self,
        registry_url: &str,
        coordinate: &str,
        version: &str,
    ) -> Option<ResolvedGenerator> {
        let path = crate::cache::generator_path(registry_url, coordinate, version).ok()?;
        let wasm = std::fs::read(path).ok()?;
        resolved_generator_from_wasm(wasm).ok()
    }

    fn write_spec_cache(&self, registry_url: &str, coordinate: &str, version: &str, wasm: &[u8]) {
        let Ok(cache_path) = crate::cache::generator_path(registry_url, coordinate, version) else {
            return;
        };
        let _ = crate::cache::write_atomic(&cache_path, wasm);
    }

    /// Compile (or read from cache) a generator at a manifest-root-relative
    /// path. Used by `LocalPath` resolution (inline paths and rewritten path
    /// build-dependencies).
    async fn resolve_local(
        &self,
        path: &InvocationPath,
    ) -> Result<ResolvedGenerator, ProviderError> {
        let (abs, _source, source_str) = self.read_local_source(path)?;
        let base = abs.parent().map(Path::to_path_buf).unwrap_or_default();
        if !self.no_cache
            && let Some(resolved) = self.try_read_cache(path.as_str(), &base)
        {
            return Ok(resolved);
        }
        // Single-flight: a parallel `wado test` resolves this generator once
        // per fixture, and every cold-cache miss would build it. `--no-cache`
        // stays unlocked — serializing rebuilds that discard each other's
        // result only adds wall time.
        let entry_lock = (!self.no_cache).then(|| build_lock(&self.index_path(), path.as_str()));
        let _building = match entry_lock.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        if !self.no_cache
            && let Some(resolved) = self.try_read_cache(path.as_str(), &base)
        {
            return Ok(resolved);
        }
        let artifacts = self
            .compile_local(path.as_str().to_string(), abs, source_str)
            .await?;
        Ok(ResolvedGenerator {
            wasm: artifacts.wasm,
            descriptor: artifacts.descriptor,
            source_hash: artifacts.source_hash,
        })
    }
}

impl GeneratorProvider for CliGeneratorProvider {
    async fn resolve(&self, module: &GeneratorModule) -> Result<ResolvedGenerator, ProviderError> {
        match module {
            // A `Spec` reaching the provider is a *registry* build-dependency:
            // a path build-dependency is rewritten to `LocalPath` up front by
            // `compile::rewrite_build_dep_modules`.
            GeneratorModule::Spec(spec) => self.resolve_spec(spec).await,
            GeneratorModule::LocalPath(path) => self.resolve_local(path).await,
        }
    }
}

impl CliGeneratorProvider {
    /// `None` on a miss or staleness — the caller recompiles. The component is
    /// content-addressed, so once [`Self::indexed_identity`] vouches for the
    /// hash there is nothing further to validate.
    fn try_read_cache(&self, module_path: &str, base: &Path) -> Option<ResolvedGenerator> {
        let identity = self.indexed_identity(module_path, base)?;
        Some(ResolvedGenerator {
            wasm: std::fs::read(self.component_path(&identity.source_hash)).ok()?,
            descriptor: identity.descriptor,
            source_hash: identity.source_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wado_compiler::kiln::InvocationPath;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn normalize_path_keeps_a_parent_chain_it_cannot_pop() {
        let cases = [
            ("./../../pkg/gen.wado", "../../pkg/gen.wado"),
            ("a/b/../c", "a/c"),
            ("../a/..", ".."),
            ("/a/b/../c", "/a/c"),
            ("/../a", "/a"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                super::normalize_path(Path::new(input)),
                Path::new(expected),
                "normalizing {input:?}"
            );
        }
    }

    /// A recorded path has to resolve back to the file it names, since it is
    /// re-hashed on every read. Entries like `.././atn.wado` resolve to
    /// nothing, so validation fails and every run rebuilds — a total cache miss
    /// that still looks like a hit.
    #[test]
    fn recorded_sources_round_trip_from_a_relative_base() {
        let cases = [
            // (base, recorded path as the host reports it, expected entry)
            ("./src", "./atn.wado", "atn.wado"),
            ("./src", "atn.wado", "atn.wado"),
            ("src", "./nested/atn.wado", "nested/atn.wado"),
            ("/pkg/src", "/pkg/src/atn.wado", "atn.wado"),
            ("/pkg/src", "/pkg/shared/atn.wado", "../shared/atn.wado"),
            ("/pkg/./src", "/pkg/src/./atn.wado", "atn.wado"),
            // A `..` chain must survive: popping one would name another
            // directory outright.
            ("../../pkg/src", "../../pkg/src/atn.wado", "atn.wado"),
            (
                "./../pkg/src",
                "./../pkg/shared/atn.wado",
                "../pkg/shared/atn.wado",
            ),
        ];
        for (base, recorded, expected) in cases {
            let base = Path::new(base);
            let out = super::make_relative_sources(base, vec![(recorded.to_string(), [0u8; 32])]);
            assert_eq!(
                out[0].0, expected,
                "base {base:?} + recorded {recorded:?} must record {expected:?}"
            );
            assert_eq!(
                super::normalize_path(&base.join(&out[0].0)),
                super::normalize_path(&base.join(recorded)),
                "the entry must resolve back to the recorded file"
            );
        }
    }

    #[test]
    fn spec_module_without_build_dependency_surfaces_unsupported() {
        // A `module: "ns:x"` spec with no matching `[build-dependencies]` entry
        // (the provider carries an empty registry context) cannot resolve.
        let provider = CliGeneratorProvider::new(PathBuf::from("/tmp"));
        let err = runtime().block_on(async {
            provider
                .resolve(&GeneratorModule::Spec("ns:x@1.0.0".into()))
                .await
                .unwrap_err()
        });
        match err {
            ProviderError::Unsupported { message } => {
                assert!(
                    message.contains("[build-dependencies]"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn directory_module_reaching_provider_surfaces_internal() {
        // A directory is resolved to its package entry up front by
        // `compile::rewrite_local_dir_modules`; one that still reaches the
        // provider declared no `core:kiln/generator` entry, so the provider
        // reports that instead of a bare "Is a directory" read error.
        let tmp = std::env::temp_dir().join(format!(
            "wado-kiln-provider-dir-module-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let pkg = tmp.join("plain-pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("wado.toml"),
            "[package]\nname = \"plain\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./plain-pkg"));
        let err = runtime()
            .block_on(async { provider.resolve(&module).await })
            .unwrap_err();
        match err {
            ProviderError::Internal { message } => {
                assert!(
                    message.contains("is a directory") && message.contains("core:kiln/generator"),
                    "unexpected message: {message}"
                );
            }
            _ => panic!("expected Internal, got {err:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_local_path_surfaces_internal() {
        let provider = CliGeneratorProvider::new(PathBuf::from("/nonexistent"));
        let err = runtime().block_on(async {
            provider
                .resolve(&GeneratorModule::LocalPath(InvocationPath::normalize(
                    "./does-not-exist",
                )))
                .await
                .unwrap_err()
        });
        match err {
            ProviderError::Internal { message } => {
                assert!(message.contains("does not exist"));
            }
            _ => panic!("expected Internal, got {err:?}"),
        }
    }

    #[test]
    fn local_path_compiles_and_caches_resolved_generator() {
        let tmp =
            std::env::temp_dir().join(format!("wado-kiln-provider-compile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let generator_src = "\
use { Request, Response, Error } from \"core:kiln\";\n\
\n\
pub struct Options {\n\
    pub verbose: bool,\n\
}\n\
\n\
export fn generate(req: Request<Options>) -> Result<Response, Error> {\n\
    let _ = req.options.verbose;\n\
    return Result::Ok(Response { files: [] });\n\
}\n";

        let gen_path = tmp.join("generator.wado");
        std::fs::write(&gen_path, generator_src).unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./generator.wado"));

        assert_eq!(provider.compile_count(), 0);

        let resolved = runtime()
            .block_on(async { provider.resolve(&module).await })
            .unwrap_or_else(|e| panic!("compile failed: {e:?}"));
        assert!(resolved.wasm.starts_with(b"\0asm"), "wasm magic missing");
        assert!(
            !resolved.source_hash.is_empty(),
            "local generators must produce a non-empty source hash"
        );
        let descriptor = resolved
            .descriptor
            .as_ref()
            .expect("Options struct present → descriptor must be Some");
        assert_eq!(descriptor.fields.len(), 1);
        assert_eq!(descriptor.fields[0].name, "verbose");
        assert_eq!(
            provider.compile_count(),
            1,
            "first resolve runs the inner compiler exactly once"
        );

        // The compile-and-write path persists the component and the index
        // entry; assert they landed so the next resolve
        // exercises the on-disk cache.
        // The compile publishes two things: the component, under its source
        // hash, and the index recording that identity.
        assert!(
            tmp.join(CACHE_DIR)
                .join(format!("{}.wasm", resolved.source_hash))
                .is_file(),
            "the component must be cached under its source hash"
        );
        let index = std::fs::read_to_string(tmp.join(INDEX_FILE)).expect("index must be written");
        assert!(
            index.contains(&resolved.source_hash),
            "the index must record the generator identity: {index}"
        );

        // Second resolve must hit the on-disk cache and skip the compile.
        // The wasm and source-hash must match byte-for-byte; the
        // descriptor's `span` info is intentionally not persisted
        // through the index (it would drift across edits), so
        // compare descriptor facets rather than the whole struct.
        let resolved2 = runtime()
            .block_on(async { provider.resolve(&module).await })
            .unwrap();
        assert_eq!(resolved.wasm, resolved2.wasm, "wasm must round-trip cache");
        assert_eq!(
            resolved.source_hash, resolved2.source_hash,
            "source_hash must round-trip cache",
        );
        let descriptor2 = resolved2
            .descriptor
            .as_ref()
            .expect("warm descriptor cache must rehydrate");
        assert_eq!(descriptor.fields.len(), descriptor2.fields.len());
        for (a, b) in descriptor.fields.iter().zip(descriptor2.fields.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.ty, b.ty);
            assert_eq!(a.default, b.default);
        }
        assert_eq!(
            provider.compile_count(),
            1,
            "warm on-disk cache must not re-invoke the inner compiler"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_after_source_edit_recompiles() {
        // Regression: keying the cache off the entry file alone
        // must invalidate when the source changes. Combined with the
        // index's transitive-file hash list, an edit to either the
        // entry or a dep must trigger exactly one fresh compile.
        let tmp = std::env::temp_dir().join(format!(
            "wado-kiln-provider-resolve-edit-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let generator_src = "\
use { Request, Response, Error } from \"core:kiln\";\n\
\n\
pub struct Options {\n\
    pub highlight: bool,\n\
    pub depth: i32,\n\
}\n\
\n\
export fn generate(req: Request<Options>) -> Result<Response, Error> {\n\
    let _ = req.options.highlight;\n\
    let _ = req.options.depth;\n\
    return Result::Ok(Response { files: [] });\n\
}\n";
        let gen_path = tmp.join("generator.wado");
        std::fs::write(&gen_path, generator_src).unwrap();

        let provider = CliGeneratorProvider::new(tmp.clone());
        let module = GeneratorModule::LocalPath(InvocationPath::normalize("./generator.wado"));

        let first = runtime()
            .block_on(async { provider.resolve(&module).await })
            .expect("first resolve");
        let first_desc = first.descriptor.expect("Options struct → Some(descriptor)");
        assert_eq!(first_desc.fields.len(), 2);
        assert_eq!(first_desc.fields[0].name, "highlight");
        assert_eq!(first_desc.fields[1].name, "depth");
        assert_eq!(provider.compile_count(), 1);

        // Second resolve, no edit: disk cache hits, no recompile.
        let _ = runtime()
            .block_on(async { provider.resolve(&module).await })
            .expect("second resolve");
        assert_eq!(provider.compile_count(), 1);

        // Source edit → fresh compile.
        let updated_src = generator_src.replace("pub depth: i32,\n", "pub depth: i64,\n");
        std::fs::write(&gen_path, &updated_src).unwrap();
        let third = runtime()
            .block_on(async { provider.resolve(&module).await })
            .expect("resolve after source edit");
        let third_desc = third.descriptor.expect("Options struct → Some(descriptor)");
        assert_eq!(third_desc.fields.len(), 2);
        assert_eq!(third_desc.fields[1].name, "depth");
        assert_eq!(
            provider.compile_count(),
            2,
            "source edit must re-compile exactly once"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
