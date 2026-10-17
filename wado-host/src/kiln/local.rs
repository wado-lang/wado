//! The Kiln pipeline for a compile on a [`StubHost`], as the e2e harness and
//! the golden dumps run one: generators named by a `./` path, compiled from
//! source, with the entry's directory as the package root.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use wado_compiler::hashmap::IndexSet;
use wado_compiler::kiln::{
    GENERATOR_WORLD_FQ, GeneratorModule, InvocationIndex, content_hash, generator_identity,
    hex_digest, remap_decl_files,
};
use wado_compiler::{Code, CompilerHost, CompilerOptions, Diagnostic, OptLevel, Severity};

use super::component_cache::ComponentCache;
use super::driver::{
    GeneratorProvider, PipelineError, ProviderError, ResolvedGenerator, emit_counting_errors,
    harvest_invocations, run_pipeline,
};
use super::runtime::generator_engine;
use crate::StubHost;
use crate::sync::lock;

/// The generator components the process shares, so a run of many fixtures
/// compiles each generator once. Give it to a host with
/// [`StubHost::with_generators`].
pub fn generators() -> Arc<ComponentCache> {
    static GENERATORS: OnceLock<Arc<ComponentCache>> = OnceLock::new();
    GENERATORS
        .get_or_init(|| Arc::new(ComponentCache::new(generator_engine)))
        .clone()
}

/// Run the generators the module graph under `entry_file`, whose text is
/// `entry_source`, declares, and answer the redirect index the compile of
/// `entry_identity` that follows takes.
///
/// The outputs land under the entry's directory, in `build/kiln/` unless a
/// clause names its own `output_dir`, as `wado` writes them for a file outside
/// any package. Every generator runs on every call: two compiles of one entry
/// write the same bytes, so they may run at once.
///
/// `None` when the pipeline failed, which `host` has been told why.
pub async fn run_local_generators(
    entry_file: &Path,
    entry_source: &str,
    entry_identity: &str,
    host: &StubHost,
) -> Option<InvocationIndex> {
    match run(entry_file, entry_source, entry_identity, host).await {
        Ok(index) => Some(index),
        // Reported clause by clause already.
        Err(PipelineError::InlineClause(_) | PipelineError::RedirectConflict(_)) => None,
        Err(error) => {
            host.emit_diagnostic(Diagnostic {
                severity: Severity::Error,
                code: Code::Log,
                message: error.to_string(),
                span: None,
            });
            None
        }
    }
}

async fn run(
    entry_file: &Path,
    entry_source: &str,
    entry_identity: &str,
    host: &StubHost,
) -> Result<InvocationIndex, PipelineError> {
    let entry_key = entry_file
        .file_name()
        .expect("an entry names a file")
        .to_string_lossy();
    let (invocations, identities, diagnostics) = harvest_invocations(
        entry_file,
        entry_source,
        entry_identity,
        &entry_key,
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

    let root = entry_file.parent().expect("an entry names a file");
    let mut provider = LocalGenerators::default();
    for invocation in &invocations {
        if !provider
            .compiled
            .iter()
            .any(|(m, _)| m == &invocation.module)
        {
            let resolved = compile_generator(root, &invocation.module).await;
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
/// what they compile to.
async fn compile_generator(
    root: &Path,
    module: &GeneratorModule,
) -> Result<ResolvedGenerator, ProviderError> {
    static COMPILED: Mutex<Vec<(PathBuf, String, Result<ResolvedGenerator, ProviderError>)>> =
        Mutex::new(Vec::new());

    let unsupported = |message: String| Err(ProviderError::Unsupported { message });
    let GeneratorModule::LocalPath(path) = module else {
        return unsupported(format!(
            "kiln: generator `{}` is not a `./` path, the only kind a fixture runs",
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
    let source = std::fs::read_to_string(&file).map_err(|e| ProviderError::Internal {
        message: format!("kiln: cannot read generator `{}`: {e}", file.display()),
    })?;
    let cached = lock(&COMPILED)
        .iter()
        .find(|(f, s, _)| f == &file && s == &source)
        .map(|(_, _, resolved)| resolved.clone());
    if let Some(resolved) = cached {
        return resolved;
    }
    let resolved = compile_generator_source(&file, path.as_str(), &source).await;
    lock(&COMPILED).push((file, source, resolved.clone()));
    resolved
}

/// `source`, the text of the generator `file`, compiled to a component. A
/// generator's own Kiln invocations are not run.
async fn compile_generator_source(
    file: &Path,
    path: &str,
    source: &str,
) -> Result<ResolvedGenerator, ProviderError> {
    let host = StubHost::new(
        file.parent()
            .expect("a generator names a file")
            .to_path_buf(),
    );
    let name = file
        .file_name()
        .expect("a generator names a file")
        .to_string_lossy();
    let options = CompilerOptions {
        opt_level: OptLevel::O1,
        target_world: Some(GENERATOR_WORLD_FQ.to_string()),
        ..CompilerOptions::default()
    };
    let compiled = wado_compiler::compile_with_options(source, &host, Some(&name), options)
        .await
        .map_err(|_| ProviderError::Internal {
            message: format!(
                "kiln: generator `{path}` does not compile:\n{}",
                host.diagnostics()
                    .iter()
                    .filter(|d| d.severity == Severity::Error)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        })?;
    Ok(ResolvedGenerator {
        source_hash: hex_digest(&content_hash(&compiled.wasm)),
        wasm: compiled.wasm,
        descriptor: compiled.kiln_options_descriptor,
    })
}
