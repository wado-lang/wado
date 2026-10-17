//! The Kiln pipeline for a compile on a [`StubHost`], as the e2e harness and
//! the golden dumps run one: generators named by a `./` path or a stubbed
//! `[build-dependencies]` nickname, compiled from source, with the entry's
//! directory as the package root.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use wado_compiler::hashmap::IndexSet;
use wado_compiler::kiln::inline::DEFAULT_INLINE_OUTPUT_DIR_PREFIX;
use wado_compiler::kiln::{
    GeneratorModule, Invocation, InvocationIndex, InvocationPath, content_hash, generator_identity,
    hex_digest, remap_decl_files, spec_key,
};
use wado_compiler::{Code, CompilerHost, Diagnostic, Severity};

use super::component_cache::ComponentCache;
use super::driver::{
    GeneratorProvider, PipelineError, ProviderError, ResolvedGenerator, emit_counting_errors,
    generator_compile_options, generator_cycle, harvest_invocations, run_pipeline,
};
use crate::sync::lock;
use crate::{HostStubs, StubHost};

/// The generator components the process shares, so a run of many fixtures
/// compiles each generator once. Give it to a host with
/// [`StubHost::with_generators`].
pub fn generators() -> Arc<ComponentCache> {
    static GENERATORS: OnceLock<Arc<ComponentCache>> = OnceLock::new();
    GENERATORS
        .get_or_init(|| Arc::new(ComponentCache::default()))
        .clone()
}

/// Run the generators the module graph under `entry_file`, whose text is
/// `entry_source`, declares, and answer the redirect index the compile of
/// `entry_identity` that follows takes.
///
/// The outputs land under the entry's directory, in `build/kiln/`, as `wado`
/// writes them for a file outside any package. A generator's own invocations
/// write there too. Every generator runs on every call: two compiles of one
/// entry write the same bytes, so they may run at once.
///
/// `None` when the pipeline failed, which `host` has been told why.
pub async fn run_local_generators(
    entry_file: &Path,
    entry_source: &str,
    entry_identity: &str,
    host: &StubHost,
) -> Option<InvocationIndex> {
    let root = entry_file.parent().expect("an entry names a file");
    let entry_key = entry_file
        .file_name()
        .expect("an entry names a file")
        .to_string_lossy();
    let entry = Entry {
        file: entry_file,
        source: entry_source,
        identity: entry_identity,
        key: &entry_key,
    };
    match run(root, &entry, host, &[]).await {
        Ok(index) => Some(index),
        Err(error) => {
            emit_pipeline_error(host, error);
            None
        }
    }
}

/// A module whose clauses a pipeline runs: its file, its text, the name the
/// compile that follows gives it, and its path from the package root.
struct Entry<'a> {
    file: &'a Path,
    source: &'a str,
    identity: &'a str,
    key: &'a str,
}

/// `chain` holds the generators being compiled around this run, outermost
/// first, by their paths from `root`.
async fn run(
    root: &Path,
    entry: &Entry<'_>,
    host: &StubHost,
    chain: &[String],
) -> Result<InvocationIndex, PipelineError> {
    let (mut invocations, identities, diagnostics) = harvest_invocations(
        entry.file,
        entry.source,
        entry.identity,
        entry.key,
        &mut IndexSet::default(),
    )
    .await;
    let clause_errors = emit_counting_errors(host, diagnostics);
    if clause_errors > 0 {
        return Err(PipelineError::InlineClause(clause_errors));
    }
    if invocations.is_empty() {
        return Ok(InvocationIndex::default());
    }
    let shared_dirs = emit_counting_errors(host, refuse_own_output_dirs(&invocations));
    if shared_dirs > 0 {
        return Err(PipelineError::InlineClause(shared_dirs));
    }
    resolve_build_dependencies(&mut invocations, host.stubs());

    let mut provider = LocalGenerators::default();
    for invocation in &invocations {
        if !provider
            .compiled
            .iter()
            .any(|(m, _)| m == &invocation.module)
        {
            let resolved = compile_generator(root, &invocation.module, host.stubs(), chain).await;
            provider
                .compiled
                .push((invocation.module.clone(), resolved));
        }
    }

    let mut index = run_pipeline(root, host, &provider, invocations, true)
        .await?
        .invocations;
    let conflicts = emit_counting_errors(host, remap_decl_files(&mut index, &identities));
    if conflicts > 0 {
        return Err(PipelineError::RedirectConflict(conflicts));
    }
    Ok(index)
}

fn emit_pipeline_error(host: &StubHost, error: PipelineError) {
    match error {
        // Reported clause by clause already.
        PipelineError::InlineClause(_) | PipelineError::RedirectConflict(_) => {}
        error => host.emit_diagnostic(Diagnostic {
            severity: Severity::Error,
            code: Code::Log,
            message: error.to_string(),
            span: None,
        }),
    }
}

/// Every fixture in a directory shares its root, and a pipeline deletes the
/// generated files in each output directory it did not write. A directory of
/// the invocation's own is one no other fixture reconciles.
fn refuse_own_output_dirs(invocations: &[Invocation]) -> Vec<Diagnostic> {
    invocations
        .iter()
        .filter(|i| {
            !i.output_dir
                .as_str()
                .starts_with(DEFAULT_INLINE_OUTPUT_DIR_PREFIX)
        })
        .map(|i| Diagnostic {
            severity: Severity::Error,
            code: Code::Log,
            message: format!(
                "kiln: `{}` names its own `output_dir`, which a fixture cannot: \
                 other fixtures in its directory would delete what it writes",
                i.from.as_str()
            ),
            span: None,
        })
        .collect()
}

/// Point each clause naming a stubbed `[build-dependencies]` nickname at the
/// generator file the stub names, as `wado` points one at its package's
/// generator entry. A nickname the stubs lack stays a spec, which no fixture
/// runs.
fn resolve_build_dependencies(invocations: &mut [Invocation], stubs: &HostStubs) {
    for invocation in invocations {
        let GeneratorModule::Spec(spec) = &invocation.module else {
            continue;
        };
        if let Some(file) = stubs.build_dependencies.get(spec_key(&spec.spec)) {
            invocation.module = GeneratorModule::LocalPath(InvocationPath::normalize(file));
        }
    }
}

/// Generators compiled before the pipeline asks for them. A compile's future
/// is not `Send`, which [`GeneratorProvider::resolve`]'s must be.
#[derive(Default)]
struct LocalGenerators {
    compiled: Vec<(GeneratorModule, Result<ResolvedGenerator, ProviderError>)>,
}

impl GeneratorProvider for LocalGenerators {
    async fn resolve(&self, module: &GeneratorModule) -> Result<ResolvedGenerator, ProviderError> {
        self.compiled
            .iter()
            .find(|(m, _)| m == module)
            .map(|(_, resolved)| resolved.clone())
            .expect("every module the invocations name was compiled")
    }
}

/// The generator `module` names, compiled once per process for each text of
/// its file: a run reads files that do not change under it, so neither does
/// what they compile to. Fixtures reaching a cold entry together each compile
/// it, which costs time once and changes no result.
///
/// Only a success is kept. A failure can depend on `chain`, as a cycle does,
/// and is cheap to reach again.
async fn compile_generator(
    root: &Path,
    module: &GeneratorModule,
    stubs: &HostStubs,
    chain: &[String],
) -> Result<ResolvedGenerator, ProviderError> {
    static COMPILED: Mutex<Vec<(PathBuf, String, ResolvedGenerator)>> = Mutex::new(Vec::new());

    let unsupported = |message: String| Err(ProviderError::Unsupported { message });
    let GeneratorModule::LocalPath(path) = module else {
        return unsupported(format!(
            "kiln: generator `{}` is neither a `./` path nor a stubbed build dependency, \
             the kinds a fixture runs",
            generator_identity(module)
        ));
    };
    let file = root.join(path.as_str());
    if file.is_dir() {
        return unsupported(format!(
            "kiln: generator `{}` is a package directory; a fixture names its `.wado` file",
            path.as_str()
        ));
    }
    if let Some(cycle) = generator_cycle(chain, path.as_str()) {
        return Err(cycle);
    }
    let source = std::fs::read_to_string(&file).map_err(|e| ProviderError::Internal {
        message: format!("kiln: cannot read generator `{}`: {e}", file.display()),
    })?;
    let cached = lock(&COMPILED)
        .iter()
        .find(|(f, s, _)| f == &file && s == &source)
        .map(|(_, _, resolved)| resolved.clone());
    if let Some(resolved) = cached {
        return Ok(resolved);
    }
    let resolved =
        compile_generator_source(root, &file, path.as_str(), &source, stubs, chain).await?;
    let mut compiled = lock(&COMPILED);
    if !compiled.iter().any(|(f, s, _)| f == &file && s == &source) {
        compiled.push((file, source, resolved.clone()));
    }
    Ok(resolved)
}

/// `source`, the text of the generator `file` at `path` from `root`, compiled
/// to a component once its own invocations have run.
async fn compile_generator_source(
    root: &Path,
    file: &Path,
    path: &str,
    source: &str,
    stubs: &HostStubs,
    chain: &[String],
) -> Result<ResolvedGenerator, ProviderError> {
    let name = file
        .file_name()
        .expect("a generator names a file")
        .to_string_lossy();
    let generator = Entry {
        file,
        source,
        identity: &name,
        key: path,
    };
    let invocations = nested_invocations(root, &generator, stubs, chain).await?;

    let host = StubHost::new(
        file.parent()
            .expect("a generator names a file")
            .to_path_buf(),
    );
    let options = generator_compile_options(invocations);
    let compiled = wado_compiler::compile_with_options(source, &host, Some(&name), options)
        .await
        .map_err(|_| ProviderError::Internal {
            message: format!(
                "kiln: generator `{path}` does not compile:\n{}",
                error_lines(&host)
            ),
        })?;
    Ok(ResolvedGenerator {
        source_hash: hex_digest(&content_hash(&compiled.wasm)),
        wasm: compiled.wasm,
        descriptor: compiled.kiln_options_descriptor,
    })
}

/// The invocations `generator` declares in its own source, run under the same
/// root as the clause that named it, so one package keeps one output tree.
async fn nested_invocations(
    root: &Path,
    generator: &Entry<'_>,
    stubs: &HostStubs,
    chain: &[String],
) -> Result<InvocationIndex, ProviderError> {
    let chain = [chain, &[generator.key.to_string()]].concat();
    let host = StubHost::new(root.to_path_buf())
        .with_stubs(stubs.clone())
        .with_generators(generators());
    Box::pin(run(root, generator, &host, &chain))
        .await
        .map_err(|error| {
            emit_pipeline_error(&host, error);
            ProviderError::Internal {
                message: format!(
                    "kiln: generator `{}` declares an invocation that failed:\n{}",
                    generator.key,
                    error_lines(&host)
                ),
            }
        })
}

fn error_lines(host: &StubHost) -> String {
    host.diagnostics()
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
