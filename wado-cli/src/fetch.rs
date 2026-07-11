//! `wado fetch` — download the project's registry dependencies.
//!
//! Resolves the dependency graph (like `wado update`, but without rewriting the
//! lock) and pulls each registry package's Component Model artifact.
//!
//! A registry dependency is a prebuilt component. At compile time the compiler
//! resolves `use … from "ns:pkg"` to a fetched component and composes it in
//! (see `dep_component`, which caches under the shared `~/wado/` tree). `wado
//! fetch` is a warm-the-cache convenience that pulls every component into that
//! same cache ahead of the build.

use std::fmt::Write as _;

use crate::args::{self, CliExit};
use crate::manifest::discover;
use crate::oci;
use crate::registry::FilesystemProvider;

#[derive(Debug)]
pub struct FetchOptions {}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado fetch [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Download the project's registry dependencies.").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    writeln!(buf, "  -h, --help  Show this help message").unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<FetchOptions, CliExit> {
    let usage = format_usage();
    if let Some(arg) = args::next_arg(&mut parser)? {
        return match arg {
            lexopt::Arg::Long("help") | lexopt::Arg::Short('h') => Err(CliExit::help(usage)),
            other => Err(args::unexpected_arg(other, &usage)),
        };
    }
    Ok(FetchOptions {})
}

pub async fn run(_opts: FetchOptions) -> Result<(), CliExit> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    let project = discover(&cwd)
        .map_err(CliExit::error)?
        .ok_or_else(|| CliExit::error("no wado.toml found"))?;
    crate::manifest::emit_manifest_warnings(&project);

    let provider = FilesystemProvider::new(project.root.clone());
    let packages = wado_manifest::resolve(&project.manifest, &provider)
        .await
        .map_err(|e| CliExit::error(format!("resolving dependencies: {e}")))?;

    let mut count = 0;
    for package in &packages {
        // A registry package carries an integrity digest; a git package a
        // resolved commit. Pull the component or materialize the worktree.
        if package.integrity.is_some() {
            let (registry_url, coordinate, _name) = split_registry_id(&package.id)
                .ok_or_else(|| CliExit::error(format!("unexpected lock id {:?}", package.id)))?;
            let out = crate::cache::component_path(registry_url, coordinate, &package.version)
                .map_err(CliExit::error)?;
            if !out.is_file() {
                let reference = oci::reference(registry_url, coordinate, &package.version)
                    .map_err(|e| CliExit::error(format!("{}: {e}", package.id)))?;
                let bytes = oci::pull_component(&reference).await.map_err(|e| {
                    CliExit::error(format!("fetching {coordinate}@{}: {e}", package.version))
                })?;
                crate::cache::write_atomic(&out, &bytes)
                    .map_err(|e| CliExit::error(format!("writing {}: {e}", out.display())))?;
            }
            eprintln!("Fetched {coordinate}@{} → {}", package.version, out.display());
            count += 1;
        } else if let Some(sha) = &package.resolved_ref {
            let url = split_git_id(&package.id)
                .ok_or_else(|| CliExit::error(format!("unexpected lock id {:?}", package.id)))?
                .to_string();
            let (version, sha) = (package.version.clone(), sha.clone());
            let url_for_msg = url.clone();
            let worktree = tokio::task::spawn_blocking(move || {
                crate::git::materialize(&url, &version, &sha)
            })
            .await
            .map_err(|e| CliExit::error(format!("materializing {}: {e}", package.id)))?
            .map_err(|e| CliExit::error(format!("materializing {}: {e}", package.id)))?;
            eprintln!(
                "Fetched {url_for_msg}@{} → {}",
                package.version,
                worktree.display()
            );
            count += 1;
        }
    }

    // Build-dependencies are Kiln generators, published at their package's
    // `core-kiln-generator` world sub-path. Pre-pull each into the generator
    // cache the Kiln provider reads at compile time. This is best-effort: the
    // provider still resolves lazily at compile time, so a build-dependency that
    // cannot be pre-fetched here (offline, missing registry) is a warning, not a
    // hard failure that would leave the just-fetched `[dependencies]` orphaned.
    // The `wado.lock` pin is preferred over a live version listing.
    let locked = crate::build_dep::locked_generator_versions(&project.root);
    let generators = match fetch_generators(&project, &locked).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("warning: skipping build-dependency pre-fetch: {e}");
            0
        }
    };

    eprintln!("Fetched {} component(s)", count + generators);
    Ok(())
}

/// Resolve and pull the project's registry build-dependencies into the generator
/// cache; returns the number pulled. Separated so `run` can treat a failure as a
/// non-fatal warning.
async fn fetch_generators(
    project: &crate::manifest::ProjectManifest,
    locked: &indexmap::IndexMap<String, String>,
) -> Result<usize, String> {
    let build_deps =
        crate::build_dep::resolve_build_dependencies(&project.manifest, locked).await?;
    let generators = crate::build_dep::fetch_build_dependencies(&build_deps).await?;
    for dep in &build_deps {
        let path = crate::cache::generator_path(&dep.registry_url, &dep.coordinate, &dep.version)?;
        eprintln!(
            "Fetched {}@{} (generator) → {}",
            dep.coordinate,
            dep.version,
            path.display()
        );
    }
    Ok(generators)
}

/// Split a registry lock id `registry+<url>/<ns>:<pkg>` into its registry URL,
/// `ns:pkg` coordinate, and bare package name. Non-registry ids yield `None`.
pub(crate) fn split_registry_id(id: &str) -> Option<(&str, &str, &str)> {
    let rest = id.strip_prefix("registry+")?;
    let (registry_url, coordinate) = rest.rsplit_once('/')?;
    let (_namespace, name) = coordinate.split_once(':')?;
    Some((registry_url, coordinate, name))
}

/// The git URL of a git lock id `git+<url>/<coordinate>`. The coordinate is the
/// trailing `ns:pkg` segment, so the final `/` splits the URL from it.
pub(crate) fn split_git_id(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("git+")?;
    let (url, _coordinate) = rest.rsplit_once('/')?;
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::split_registry_id;

    #[test]
    fn splits_a_registry_id() {
        let (url, coord, name) =
            split_registry_id("registry+oci://ghcr.io/wado-lang:cm-catalog").unwrap();
        assert_eq!(url, "oci://ghcr.io");
        assert_eq!(coord, "wado-lang:cm-catalog");
        assert_eq!(name, "cm-catalog");
    }

    #[test]
    fn splits_a_registry_id_with_prefix() {
        let (url, coord, name) = split_registry_id("registry+oci://ghcr.io/acme/ns:pkg").unwrap();
        assert_eq!(url, "oci://ghcr.io/acme");
        assert_eq!(coord, "ns:pkg");
        assert_eq!(name, "pkg");
    }

    #[test]
    fn ignores_non_registry_id() {
        assert!(split_registry_id("git+https://example.com/foo/ns:pkg").is_none());
    }
}
