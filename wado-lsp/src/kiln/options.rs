//! Typed check of an inline `with { generator: { options: … } }` table against
//! the generator's `pub struct Options`.
//!
//! The LSP cannot run a generator, but it does not need to: the descriptor is a
//! property of the generator's *source*, which the host can read and the
//! frontend can analyze. So the editor reports an unknown, missing or
//! mistyped option on the key that is wrong, without a `wado compile` in
//! between.
//!
//! A generator whose source the LSP cannot reach — a registry
//! `[build-dependencies]` component, an unreadable package — yields no
//! descriptor and therefore no verdict: saying nothing is right, since the
//! options may well be correct. `wado check` resolves those components and
//! reports them.

use std::path::{Path, PathBuf};

use wado_compiler::CompilerHost;
use wado_compiler::kiln::{
    GENERATOR_WORLD_FQ, GeneratorModule, Invocation, InvocationIndex, OptionsAnchor,
    OptionsDescriptor, extract_options_descriptor, spec_key, validate_options,
};
use wado_compiler::semantics::semantics_for_world;
use wado_manifest::DependencySource;

use crate::DiagnosticCollector;

/// Validate `invocation`'s options and emit every complaint through `host`.
/// A no-op for a generator the LSP cannot read in source.
pub(super) async fn check<H: CompilerHost>(
    invocation: &Invocation,
    manifest_root: &Path,
    host: &H,
) {
    let Some(entry) = generator_entry(invocation, manifest_root, host).await else {
        return;
    };
    let Some(descriptor) = descriptor_of(&entry, host).await else {
        return;
    };
    let anchor = OptionsAnchor {
        file: &invocation.decl_site.module,
        span: invocation.options_span,
    };
    if let Err(diagnostics) = validate_options(&descriptor, invocation.raw_options.as_ref(), anchor)
    {
        for d in diagnostics {
            host.emit_diagnostic(d);
        }
    }
}

/// The generator's entry `.wado` file, absolute. Mirrors the rewrites
/// `wado-cli` performs before its pipeline runs: a `module:` naming a package
/// directory — inline or through a path `[build-dependencies]` entry —
/// resolves to that package's `[world]."core:kiln/generator"` entry.
///
/// Joined plainly, not through [`super::safe_join`]: that sandbox is for paths
/// a *cache artifact* names, which the LSP must not follow out of the
/// workspace. These paths the user wrote in their own source and manifest —
/// `../gen` is an ordinary sibling package — and the loader already follows the
/// same spellings for a plain `use`.
async fn generator_entry<H: CompilerHost>(
    invocation: &Invocation,
    manifest_root: &Path,
    host: &H,
) -> Option<PathBuf> {
    match &invocation.module {
        GeneratorModule::LocalPath(path) => {
            let abs = manifest_root.join(path.as_str());
            match package_generator_entry(&abs, host).await {
                Some(entry) => Some(entry),
                None => Some(abs),
            }
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
/// `pkg_dir` is not a package directory (an inline `module:` naming a file
/// takes that path) or declares no generator world.
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

/// Analyze the generator's source and describe its `Options` struct.
///
/// The analysis runs under the generator world, exactly as `wado compile`
/// analyzes it, and through a silencing host: the generator's own diagnostics
/// belong to its own file, and a span-less one would otherwise surface at the
/// top of the consumer the user is editing.
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
