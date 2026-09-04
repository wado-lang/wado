//! Checks an inline `with { generator: { options: … } }` table against the
//! generator's `pub struct Options`, read from the generator's source.
//!
//! No generator runs: the descriptor is a property of that source. A generator
//! the host cannot read in source yields no descriptor and no verdict, since
//! the options may well be correct. `wado check` reports those.

use std::path::{Path, PathBuf};

use wado_compiler::CompilerHost;
use wado_compiler::hashmap::IndexMap;
use wado_compiler::kiln::{
    GENERATOR_WORLD_FQ, GeneratorModule, Invocation, InvocationIndex, OptionsAnchor,
    OptionsDescriptor, extract_options_descriptor, spec_key, validate_options,
};
use wado_compiler::semantics::semantics_for_world;
use wado_manifest::DependencySource;

use crate::DiagnosticCollector;

/// Validate every invocation's options and emit each complaint through `host`.
/// A no-op for a generator the LSP cannot read in source.
///
/// Describing a generator means analyzing its whole source, so each distinct
/// entry file is described once and every clause naming it shares the verdict —
/// several `use` clauses through one generator is the ordinary case.
pub(super) async fn check_all<H: CompilerHost>(
    invocations: &[Invocation],
    manifest_root: &Path,
    host: &H,
) {
    let mut described: IndexMap<PathBuf, Option<OptionsDescriptor>> = IndexMap::default();
    for invocation in invocations {
        let Some(entry) = generator_entry(invocation, manifest_root, host).await else {
            continue;
        };
        if !described.contains_key(&entry) {
            let descriptor = descriptor_of(&entry, host).await;
            described.insert(entry.clone(), descriptor);
        }
        let Some(descriptor) = described[&entry].as_ref() else {
            continue;
        };
        let anchor = OptionsAnchor {
            file: &invocation.decl_site.module,
            span: invocation.options_span,
        };
        if let Err(diagnostics) =
            validate_options(descriptor, invocation.raw_options.as_ref(), anchor)
        {
            for d in diagnostics {
                host.emit_diagnostic(d);
            }
        }
    }
}

/// The generator's entry `.wado` file, absolute, resolved as `wado-cli`
/// resolves it before its pipeline runs.
///
/// Joined plainly, not through [`super::safe_join`]: that sandbox is for the
/// paths a cache artifact names. These the user wrote in their own source and
/// manifest, where `../gen` is an ordinary sibling package, and the loader
/// follows the same spellings for a plain `use`.
async fn generator_entry<H: CompilerHost>(
    invocation: &Invocation,
    manifest_root: &Path,
    host: &H,
) -> Option<PathBuf> {
    match &invocation.module {
        GeneratorModule::LocalPath(path) => {
            let abs = manifest_root.join(path.as_str());
            Some(package_generator_entry(&abs, host).await.unwrap_or(abs))
        }
        GeneratorModule::Spec(spec) => {
            let manifest = read_manifest(manifest_root, host).await?;
            let dep = manifest.build_dependencies.get(spec_key(&spec.spec))?;
            let DependencySource::Path { path, .. } = &dep.source else {
                return None;
            };
            package_generator_entry(&manifest_root.join(path), host).await
        }
    }
}

/// `pkg_dir`'s `[world]."core:kiln/generator"` entry, absolute. `None` when
/// `pkg_dir` is a file rather than a package, or declares no generator world.
async fn package_generator_entry<H: CompilerHost>(pkg_dir: &Path, host: &H) -> Option<PathBuf> {
    let manifest = read_manifest(pkg_dir, host).await?;
    Some(pkg_dir.join(manifest.world_entry(GENERATOR_WORLD_FQ)?))
}

/// Parse `dir`'s `wado.toml`, applying `[workspace.package]` inheritance —
/// a generator package is usually a workspace member, and a member manifest
/// does not parse standalone.
async fn read_manifest<H: CompilerHost>(dir: &Path, host: &H) -> Option<wado_manifest::Manifest> {
    let path = dir.join(wado_manifest::MANIFEST_FILENAME);
    let bytes = host.load_source(&path.display().to_string()).await.ok()?;
    let content = String::from_utf8(bytes).ok()?;
    crate::host::discovery::resolve_member_manifest(dir, &content).ok()
}

/// Analyze the generator's source, under the generator world as `wado compile`
/// analyzes it, and describe its `Options` struct.
///
/// The host is silenced: the generator's own diagnostics belong to its own
/// file, and a span-less one would surface at the top of the open document.
async fn descriptor_of<H: CompilerHost>(entry: &Path, host: &H) -> Option<OptionsDescriptor> {
    let filename = entry.display().to_string();
    let source = String::from_utf8(host.load_source(&filename).await.ok()?).ok()?;
    let quiet = DiagnosticCollector::silencing(host);
    let sem = semantics_for_world(
        &source,
        &quiet,
        Some(&filename),
        Some(GENERATOR_WORLD_FQ),
        InvocationIndex::new(),
    )
    .await;
    extract_options_descriptor(&sem, &sem.entry_module_source).ok()
}
