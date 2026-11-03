//! The Kiln pipeline for a compile on a [`StubHost`], as the e2e harness and
//! the golden dumps run one: generators named by a `./` path or a stubbed
//! `[build-dependencies]` nickname, compiled from source, with the entry's
//! directory as the package root.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use indexmap::IndexMap;
use wado_compiler::hashmap::IndexSet;
use wado_compiler::kiln::inline::default_output_dir;
use wado_compiler::kiln::{
    GeneratorModule, Invocation, InvocationIndex, InvocationPath, content_hash, generator_identity,
    hex_digest, remap_decl_files,
};
use wado_compiler::{Code, CompilerHost, Diagnostic, Severity};

use super::driver::{
    GeneratorProvider, Harvested, PipelineError, ProviderError, ResolvedGenerator,
    emit_counting_errors, generator_compile_options, generator_cycle, harvest_invocations,
    merge_across_packages, run_pipeline,
};
use crate::sync::lock;
use crate::{HostStubs, StubHost};

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
    let Harvested {
        mut invocations,
        identities,
        diagnostics,
        ..
    } = harvest_invocations(
        entry.file,
        entry.source,
        entry.identity,
        entry.key,
        &mut IndexSet::default(),
        &host.dependency_index(),
        host,
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
    let invocations = merge_across_packages(invocations).map_err(|disagreements| {
        PipelineError::InlineClause(emit_counting_errors(host, disagreements))
    })?;

    let mut provider = LocalGenerators::default();
    for invocation in &invocations {
        if !provider
            .compiled
            .iter()
            .any(|(m, _)| m == &invocation.module)
        {
            let resolved = compile_generator(root, &invocation.module, host, chain).await;
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
    if !error.is_reported() {
        host.emit_diagnostic(Diagnostic {
            severity: Severity::Error,
            code: Code::Log,
            message: error.to_string(),
            span: None,
        });
    }
}

/// Every fixture in a directory shares its root, and a pipeline deletes the
/// generated files in each output directory it did not write. Only the
/// directory a clause gets by default is its own.
fn refuse_own_output_dirs(invocations: &[Invocation]) -> Vec<Diagnostic> {
    invocations
        .iter()
        .filter(|i| {
            !i.decl_sites
                .iter()
                .any(|site| i.output_dir == default_output_dir(&site.synthetic_id))
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
/// generator entry. A stub matches the spec as written, so a version or a
/// submodule the stub does not spell leaves it a spec, which no fixture runs.
fn resolve_build_dependencies(invocations: &mut [Invocation], stubs: &HostStubs) {
    for invocation in invocations {
        let GeneratorModule::Spec(spec) = &invocation.module else {
            continue;
        };
        if spec.version.is_some() {
            continue;
        }
        if let Some(file) = stubs.build_dependencies.get(&spec.spec) {
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
    host: &StubHost,
    chain: &[String],
) -> Result<ResolvedGenerator, ProviderError> {
    static COMPILED: Mutex<Vec<(CompileKey, ResolvedGenerator)>> = Mutex::new(Vec::new());

    // Unsupported is the pipeline's word for a generator this host cannot
    // run, which it lets pass as stale. These are a fixture's mistakes.
    let refused = |message: String| Err(ProviderError::Internal { message });
    let GeneratorModule::LocalPath(path) = module else {
        return refused(format!(
            "kiln: generator `{}` is neither a `./` path nor a stubbed build dependency, \
             the kinds a fixture runs",
            generator_identity(module)
        ));
    };
    let file = root.join(path.as_str());
    if file.is_dir() {
        return refused(format!(
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
    // The generator's own clauses resolve their nicknames through the stubs.
    let key = CompileKey {
        file,
        source,
        build_dependencies: host.stubs().build_dependencies.clone(),
        reads_manifests: host.reads_manifests(),
    };
    let cached = lock(&COMPILED)
        .iter()
        .find(|(k, _)| k == &key)
        .map(|(_, resolved)| resolved.clone());
    if let Some(resolved) = cached {
        return Ok(resolved);
    }
    let resolved =
        compile_generator_source(root, &key.file, path.as_str(), &key.source, host, chain).await?;
    let mut compiled = lock(&COMPILED);
    if !compiled.iter().any(|(k, _)| k == &key) {
        compiled.push((key, resolved.clone()));
    }
    Ok(resolved)
}

/// What a compiled generator is a function of.
#[derive(PartialEq)]
struct CompileKey {
    file: PathBuf,
    source: String,
    build_dependencies: IndexMap<String, String>,
    reads_manifests: bool,
}

/// `source`, the text of the generator `file` at `path` from `root`, compiled
/// to a component once its own invocations have run.
async fn compile_generator_source(
    root: &Path,
    file: &Path,
    path: &str,
    source: &str,
    host: &StubHost,
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
    let invocations = nested_invocations(root, &generator, host, chain).await?;

    let host = host.for_generator(
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
    host: &StubHost,
    chain: &[String],
) -> Result<InvocationIndex, ProviderError> {
    let chain = [chain, &[generator.key.to_string()]].concat();
    let generator_dir = generator.file.parent().expect("a generator names a file");
    let host = host.for_generator_clauses(root.to_path_buf(), generator_dir);
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
