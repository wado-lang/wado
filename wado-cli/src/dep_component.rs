//! Fetch registry dependencies as prebuilt Component Model artifacts and map
//! each specifier to its local `.wasm`, so the compiler resolves
//! `use { X } from "ns:pkg"` across the CM boundary (dependency-management
//! Phase 4). A registry dependency is a standalone component — unlike a path
//! dependency, which compiles into the consumer — so it is imported like a
//! `with { type: "wasm" }` asset, not compiled from source.
//!
//! Two source forms feed the same index:
//!
//! - A `[dependencies]` table entry ([`fetch_component_dependencies`]) —
//!   resolved through `wado.lock`, keyed by the bare coordinate.
//! - A single-file inline `use … from "ns:pkg@ver" with { registry }` clause
//!   ([`fetch_inline_component_dependencies`]) — no lock, an exact pin, keyed by
//!   the verbatim specifier.
//!
//! Components are cached under `<base>/build/deps/`; a published version is
//! immutable, so a present cache file is reused without a re-pull.

use std::path::Path;

use wado_manifest::Manifest;

use crate::oci;
use crate::registry::FilesystemProvider;

/// Directory (under the base dir) where fetched component dependencies land.
const DEPS_CACHE_DIR: &str = "build/deps";

/// Resolve and fetch every registry `[dependencies]` entry into the component
/// cache, returning `(coordinate "ns:pkg", absolute local .wasm path)` pairs.
/// Path dependencies (which compile from source) and non-registry sources are
/// ignored. The pairs are merged into the compiler's dependency index.
pub async fn fetch_component_dependencies(
    manifest: &Manifest,
    manifest_dir: &Path,
) -> Result<Vec<(String, String)>, String> {
    let provider = FilesystemProvider::new(manifest_dir.to_path_buf());
    let packages = wado_manifest::resolve(manifest, &provider)
        .await
        .map_err(|e| format!("resolving dependencies: {e}"))?;

    let deps_dir = manifest_dir.join(DEPS_CACHE_DIR);
    let mut out = Vec::new();
    for package in packages.iter().filter(|p| p.integrity.is_some()) {
        let (registry_url, coordinate, name) = crate::fetch::split_registry_id(&package.id)
            .ok_or_else(|| format!("unexpected lock id {:?}", package.id))?;
        let abs = pull_component_into(registry_url, coordinate, name, &package.version, &deps_dir)
            .await?;
        out.push((coordinate.to_string(), abs));
    }
    Ok(out)
}

/// Resolve and fetch every inline `use … from "ns:pkg@ver" with { registry }`
/// clause in `source` (single-file mode; the manifest, when present, supplies a
/// default registry and enforces the inline-vs-`[dependencies]` exclusivity).
/// Returns `(verbatim specifier, absolute local .wasm path)` pairs — keyed by
/// the specifier the loader looks up, so a `@ver` pin round-trips.
pub async fn fetch_inline_component_dependencies(
    source: &str,
    manifest: Option<&Manifest>,
    base_dir: &Path,
) -> Result<Vec<(String, String)>, String> {
    let deps = collect_inline_deps(source, manifest)?;
    let deps_dir = base_dir.join(DEPS_CACHE_DIR);
    let mut out = Vec::new();
    for dep in deps {
        let abs = pull_component_into(
            &dep.registry_url,
            &dep.coordinate,
            &dep.name,
            &dep.version,
            &deps_dir,
        )
        .await?;
        out.push((dep.specifier, abs));
    }
    Ok(out)
}

/// An inline registry component source declared on a `use … from` clause.
#[derive(Debug)]
struct InlineDep {
    /// Verbatim `from "…"` specifier — the components-index key the loader
    /// looks up (carries any `@ver` pin).
    specifier: String,
    coordinate: String,
    name: String,
    registry_url: String,
    version: String,
}

/// Parse `source` and collect its inline registry component dependencies. A
/// partial parse (mid-edit source) yields none — the real compile reports the
/// syntax error.
fn collect_inline_deps(
    source: &str,
    manifest: Option<&Manifest>,
) -> Result<Vec<InlineDep>, String> {
    let Ok(parsed) = wado_compiler::parse(source).into_fail_fast() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in &parsed.ast.items {
        let wado_compiler::ast::Item::Use(use_decl) = item else {
            continue;
        };
        if let Some(dep) = parse_inline_dep(use_decl, manifest)? {
            out.push(dep);
        }
    }
    Ok(out)
}

/// Classify one `use` clause. Returns `None` for a non-coordinate source, or a
/// coordinate with no inline source (manifest-resolved or a resolution-time
/// error); `Some` for an inline registry source; `Err` for a malformed one.
fn parse_inline_dep(
    use_decl: &wado_compiler::ast::UseDecl,
    manifest: Option<&Manifest>,
) -> Result<Option<InlineDep>, String> {
    let specifier = use_decl.source.clone();
    let Some((coordinate, spec_version)) = split_open_coordinate(&specifier) else {
        return Ok(None);
    };
    let coordinate = coordinate.to_string();
    let spec_version = spec_version.map(str::to_string);
    let attrs = use_decl.attributes.as_ref();
    let inline_registry = attrs.and_then(wado_compiler::ast::ImportAttributes::registry);
    let inline_version = attrs.and_then(wado_compiler::ast::ImportAttributes::version);

    // A bare coordinate with no inline source is resolved through the manifest
    // (or is a resolution-time error in single-file mode) — not our concern.
    if spec_version.is_none() && inline_registry.is_none() && inline_version.is_none() {
        return Ok(None);
    }

    let in_manifest = manifest.is_some_and(|m| m.dependencies.contains_key(coordinate.as_str()));
    if in_manifest {
        return Err(format!(
            "dependency {coordinate:?}: an inline `with` and a [dependencies] entry \
             are mutually exclusive"
        ));
    }

    let version = match (spec_version.as_deref(), inline_version.as_deref()) {
        (Some(v), None) | (None, Some(v)) => v.to_string(),
        (Some(_), Some(_)) => {
            return Err(format!(
                "dependency {coordinate:?}: version given both in the specifier (`@…`) \
                 and in `with {{ version }}` — use one"
            ));
        }
        (None, None) => {
            return Err(format!(
                "dependency {coordinate:?} needs a version (pin it as `{coordinate}@<version>` \
                 or `with {{ version: \"<version>\" }}`)"
            ));
        }
    };
    if semver::Version::parse(&version).is_err() {
        return Err(format!(
            "dependency {coordinate:?}: version {version:?} must be exact \
             (single-file has no lock to resolve a range)"
        ));
    }

    let registry_url = inline_registry
        .or_else(|| manifest.and_then(|m| m.registries.get("default").cloned()))
        .ok_or_else(|| {
            format!(
                "dependency {coordinate:?} needs a registry \
                 (add `with {{ registry: \"oci://…\" }}`)"
            )
        })?;

    let name = coordinate
        .split_once(':')
        .map_or_else(|| coordinate.clone(), |(_, n)| n.to_string());

    Ok(Some(InlineDep {
        specifier,
        coordinate,
        name,
        registry_url,
        version,
    }))
}

/// Split an open-namespace coordinate specifier into `(coordinate, version)`.
/// Returns `None` for reserved namespaces (`core`/`wasi`/`lib`), paths, and
/// remote URLs — anything that is not an external registry coordinate.
fn split_open_coordinate(spec: &str) -> Option<(&str, Option<&str>)> {
    if spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with("http://")
        || spec.starts_with("https://")
    {
        return None;
    }
    let (coordinate, version) = match spec.split_once('@') {
        Some((c, v)) if c.contains(':') => (c, Some(v)),
        _ => (spec, None),
    };
    let (namespace, _pkg) = coordinate.split_once(':')?;
    if matches!(namespace, "core" | "wasi" | "lib") {
        return None;
    }
    Some((coordinate, version))
}

/// Pull `coordinate` @ `version` from `registry_url` into `deps_dir`, returning
/// the absolute path. A present cache file (immutable published version) is
/// reused without a re-pull.
async fn pull_component_into(
    registry_url: &str,
    coordinate: &str,
    name: &str,
    version: &str,
    deps_dir: &Path,
) -> Result<String, String> {
    let out_path = deps_dir.join(format!("{name}-{version}.wasm"));
    if !out_path.is_file() {
        let reference = oci::reference(registry_url, coordinate, version)
            .map_err(|e| format!("{coordinate}@{version}: {e}"))?;
        let bytes = oci::pull_component(&reference)
            .await
            .map_err(|e| format!("fetching {coordinate}@{version}: {e}"))?;
        std::fs::create_dir_all(deps_dir)
            .map_err(|e| format!("creating {}: {e}", deps_dir.display()))?;
        std::fs::write(&out_path, &bytes)
            .map_err(|e| format!("writing {}: {e}", out_path.display()))?;
    }
    Ok(out_path
        .canonicalize()
        .unwrap_or(out_path)
        .display()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn use_decls(source: &str) -> Vec<wado_compiler::ast::UseDecl> {
        wado_compiler::parse(source)
            .into_fail_fast()
            .unwrap()
            .ast
            .items
            .into_iter()
            .filter_map(|item| match item {
                wado_compiler::ast::Item::Use(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    fn only(source: &str, manifest: Option<&Manifest>) -> Result<Option<InlineDep>, String> {
        let decls = use_decls(source);
        parse_inline_dep(&decls[0], manifest)
    }

    #[test]
    fn skips_reserved_and_relative_specifiers() {
        assert!(split_open_coordinate("core:cli").is_none());
        assert!(split_open_coordinate("wasi:filesystem").is_none());
        assert!(split_open_coordinate("lib:router").is_none());
        assert!(split_open_coordinate("./utils.wado").is_none());
        assert!(split_open_coordinate("https://example.com/x.wado").is_none());
    }

    #[test]
    fn splits_coordinate_and_version() {
        assert_eq!(split_open_coordinate("ns:pkg"), Some(("ns:pkg", None)));
        assert_eq!(
            split_open_coordinate("ns:pkg@1.2.3"),
            Some(("ns:pkg", Some("1.2.3")))
        );
    }

    #[test]
    fn bare_coordinate_is_manifest_resolved() {
        let dep = only(r#"use { X } from "ns:pkg";"#, None).unwrap();
        assert!(dep.is_none());
    }

    #[test]
    fn pins_version_from_specifier() {
        let dep = only(
            r#"use { X } from "ns:pkg@1.2.3" with { registry: "oci://ghcr.io" };"#,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(dep.coordinate, "ns:pkg");
        assert_eq!(dep.name, "pkg");
        assert_eq!(dep.version, "1.2.3");
        assert_eq!(dep.registry_url, "oci://ghcr.io");
        assert_eq!(dep.specifier, "ns:pkg@1.2.3");
    }

    #[test]
    fn pins_version_from_with_clause() {
        let dep = only(
            r#"use { X } from "ns:pkg" with { registry: "oci://ghcr.io", version: "1.2.3" };"#,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(dep.version, "1.2.3");
        assert_eq!(dep.specifier, "ns:pkg");
    }

    #[test]
    fn rejects_double_version() {
        let err = only(
            r#"use { X } from "ns:pkg@1.2.3" with { registry: "oci://ghcr.io", version: "1.2.3" };"#,
            None,
        )
        .unwrap_err();
        assert!(err.contains("use one"), "{err}");
    }

    #[test]
    fn rejects_range_version() {
        let err = only(
            r#"use { X } from "ns:pkg@^1.0" with { registry: "oci://ghcr.io" };"#,
            None,
        )
        .unwrap_err();
        assert!(err.contains("must be exact"), "{err}");
    }

    #[test]
    fn requires_a_registry() {
        let err = only(r#"use { X } from "ns:pkg@1.2.3";"#, None).unwrap_err();
        assert!(err.contains("needs a registry"), "{err}");
    }

    #[test]
    fn requires_a_version() {
        let err = only(
            r#"use { X } from "ns:pkg" with { registry: "oci://ghcr.io" };"#,
            None,
        )
        .unwrap_err();
        assert!(err.contains("needs a version"), "{err}");
    }

    fn manifest(toml: &str) -> Manifest {
        toml.parse().unwrap()
    }

    #[test]
    fn inline_and_manifest_entry_are_exclusive() {
        let m = manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n\n\
             [registries]\ndefault=\"oci://ghcr.io\"\n\n\
             [dependencies]\n\"ns:pkg\" = { version = \"^1.0\" }\n",
        );
        let err = only(
            r#"use { X } from "ns:pkg@1.2.3" with { registry: "oci://ghcr.io" };"#,
            Some(&m),
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn registry_falls_back_to_manifest_default() {
        let m = manifest(
            "[package]\nname=\"a\"\nversion=\"0.1.0\"\n\n\
             [registries]\ndefault=\"oci://ghcr.io/acme\"\n",
        );
        let dep = only(r#"use { X } from "ns:pkg@1.2.3";"#, Some(&m))
            .unwrap()
            .unwrap();
        assert_eq!(dep.registry_url, "oci://ghcr.io/acme");
    }
}
