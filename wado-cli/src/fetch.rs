//! `wado fetch` — download the project's registry dependencies.
//!
//! Resolves the dependency graph (like `wado update`, but without rewriting the
//! lock) and pulls each registry package's Component Model artifact.
//!
//! Bridge note: a registry dependency is a prebuilt component, but the compiler
//! cannot yet resolve a `use … from "ns:pkg"` import to a fetched component
//! (Phase 4). Until then a project imports the component as a local wasm asset
//! (`from "./build/<name>.wasm" with { type: "wasm" }`), so `fetch` writes each
//! component to `<root>/build/<name>.wasm`. Once import resolution lands, this
//! moves to the shared `~/wado` cache.

use std::fmt::Write as _;
use std::fs;

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

    let build_dir = project.root.join("build");
    let mut count = 0;
    for package in packages.iter().filter(|p| p.integrity.is_some()) {
        let (registry_url, coordinate, name) = split_registry_id(&package.id)
            .ok_or_else(|| CliExit::error(format!("unexpected lock id {:?}", package.id)))?;
        let reference = oci::reference(registry_url, coordinate, &package.version)
            .map_err(|e| CliExit::error(format!("{}: {e}", package.id)))?;
        let bytes = oci::pull_component(&reference).await.map_err(|e| {
            CliExit::error(format!("fetching {coordinate}@{}: {e}", package.version))
        })?;
        fs::create_dir_all(&build_dir)
            .map_err(|e| CliExit::error(format!("creating {}: {e}", build_dir.display())))?;
        let out = build_dir.join(format!("{name}.wasm"));
        fs::write(&out, &bytes)
            .map_err(|e| CliExit::error(format!("writing {}: {e}", out.display())))?;
        eprintln!(
            "Fetched {coordinate}@{} → {}",
            package.version,
            out.display()
        );
        count += 1;
    }

    // Build-dependencies are Kiln generators, published at their package's
    // `core-kiln-generator` world sub-path. Pull each into the generator cache
    // the Kiln provider reads at compile time.
    let build_deps = crate::build_dep::resolve_build_dependencies(&project.manifest)
        .await
        .map_err(|e| CliExit::error(format!("resolving build-dependencies: {e}")))?;
    let generators = crate::build_dep::fetch_build_dependencies(&build_deps, &project.root)
        .await
        .map_err(CliExit::error)?;
    for dep in &build_deps {
        eprintln!(
            "Fetched {}@{} (generator) → {}",
            dep.coordinate,
            dep.version,
            crate::build_dep::generator_cache_path(&project.root, &dep.coordinate).display()
        );
    }

    eprintln!("Fetched {} component(s)", count + generators);
    Ok(())
}

/// Split a registry lock id `registry+<url>/<ns>:<pkg>` into its registry URL,
/// `ns:pkg` coordinate, and bare package name. Non-registry ids yield `None`.
fn split_registry_id(id: &str) -> Option<(&str, &str, &str)> {
    let rest = id.strip_prefix("registry+")?;
    let (registry_url, coordinate) = rest.rsplit_once('/')?;
    let (_namespace, name) = coordinate.split_once(':')?;
    Some((registry_url, coordinate, name))
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
