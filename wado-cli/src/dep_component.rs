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
/// cache, returning `(specifier, absolute local .wasm path)` pairs. The
/// specifier is the manifest key the loader looks up — the bare coordinate for
/// a direct `ns:pkg` entry, or the `lib:nick` for an alias (whose `package`
/// gives the fetched coordinate). Path dependencies (which compile from source)
/// and non-registry sources are ignored.
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
        // A registry dependency is a leaf (no transitive Wado deps), so the
        // loader only ever imports it by the consuming manifest's key. Map its
        // coordinate back to that key so a `lib:` alias resolves under its
        // nickname.
        let specifier = manifest_key_for_coordinate(manifest, coordinate).unwrap_or(coordinate);
        out.push((specifier.to_string(), abs));
    }
    Ok(out)
}

/// The `[dependencies]` key backing a registry `coordinate` (the bare
/// coordinate itself, or a `lib:nick` whose `package` is that coordinate).
fn manifest_key_for_coordinate<'a>(manifest: &'a Manifest, coordinate: &str) -> Option<&'a str> {
    manifest
        .dependencies
        .iter()
        .find_map(|(key, dep)| match &dep.source {
            wado_manifest::DependencySource::Registry { package, .. } if package == coordinate => {
                Some(key.as_str())
            }
            _ => None,
        })
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

/// Classify one `use` clause. Returns `None` for a bundled/relative/remote
/// source, or a coordinate with no inline source (manifest-resolved or a
/// resolution-time error); `Some` for an inline registry source; `Err` for a
/// malformed one.
///
/// An open coordinate `ns:pkg` fetches itself; a `lib:nick` alias fetches the
/// coordinate named by `with { package }` while keeping `lib:nick` as the
/// loader's lookup key.
fn parse_inline_dep(
    use_decl: &wado_compiler::ast::UseDecl,
    manifest: Option<&Manifest>,
) -> Result<Option<InlineDep>, String> {
    let specifier = use_decl.source.clone();
    let Some((namespace, head, spec_version)) = classify_specifier(&specifier) else {
        return Ok(None);
    };
    let is_alias = namespace == "lib";
    let head = head.to_string();
    let spec_version = spec_version.map(str::to_string);
    let attrs = use_decl.attributes.as_ref();
    let inline_registry = attrs.and_then(wado_compiler::ast::ImportAttributes::registry);
    let inline_version = attrs.and_then(wado_compiler::ast::ImportAttributes::version);
    let inline_package = attrs.and_then(wado_compiler::ast::ImportAttributes::package);

    // No inline source (`package` is a source marker for an alias) → resolved
    // through the manifest, or a resolution-time error — not our concern.
    if spec_version.is_none()
        && inline_registry.is_none()
        && inline_version.is_none()
        && inline_package.is_none()
    {
        return Ok(None);
    }

    if manifest.is_some_and(|m| m.dependencies.contains_key(head.as_str())) {
        return Err(format!(
            "dependency {head:?}: an inline `with` and a [dependencies] entry \
             are mutually exclusive"
        ));
    }

    let coordinate = if is_alias {
        inline_package.ok_or_else(|| {
            format!("dependency {head:?}: a `lib:` alias needs `with {{ package: \"ns:pkg\" }}`")
        })?
    } else if inline_package.is_some() {
        return Err(format!(
            "dependency {head:?}: `package` aliasing is forbidden under an open namespace \
             (use a `lib:` specifier)"
        ));
    } else {
        head.clone()
    };

    let version = match (spec_version.as_deref(), inline_version.as_deref()) {
        (Some(v), None) | (None, Some(v)) => v.to_string(),
        (Some(_), Some(_)) => {
            return Err(format!(
                "dependency {head:?}: version given both in the specifier (`@…`) \
                 and in `with {{ version }}` — use one"
            ));
        }
        (None, None) => {
            return Err(format!(
                "dependency {head:?} needs a version \
                 (pin it as `@<version>` or `with {{ version: \"<version>\" }}`)"
            ));
        }
    };
    if semver::Version::parse(&version).is_err() {
        return Err(format!(
            "dependency {head:?}: version {version:?} must be exact \
             (single-file has no lock to resolve a range)"
        ));
    }

    let registry_url = inline_registry
        .or_else(|| manifest.and_then(|m| m.registries.get("default").cloned()))
        .ok_or_else(|| {
            format!("dependency {head:?} needs a registry (add `with {{ registry: \"oci://…\" }}`)")
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

/// Split a specifier into `(namespace, head, version)`, where `head` is the
/// specifier without any `@ver` pin. Returns `None` for bundled namespaces
/// (`core`/`wasi`), paths, and remote URLs — anything not resolved from a
/// registry. `lib:` is included: it is reserved but registry-resolvable via an
/// alias.
fn classify_specifier(spec: &str) -> Option<(&str, &str, Option<&str>)> {
    if spec.starts_with("./")
        || spec.starts_with("../")
        || spec.starts_with("http://")
        || spec.starts_with("https://")
    {
        return None;
    }
    let (head, version) = match spec.split_once('@') {
        Some((h, v)) if h.contains(':') => (h, Some(v)),
        _ => (spec, None),
    };
    let (namespace, _rest) = head.split_once(':')?;
    if matches!(namespace, "core" | "wasi") {
        return None;
    }
    Some((namespace, head, version))
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
    fn skips_bundled_and_relative_specifiers() {
        assert!(classify_specifier("core:cli").is_none());
        assert!(classify_specifier("wasi:filesystem").is_none());
        assert!(classify_specifier("./utils.wado").is_none());
        assert!(classify_specifier("https://example.com/x.wado").is_none());
    }

    #[test]
    fn classifies_coordinate_and_alias() {
        assert_eq!(classify_specifier("ns:pkg"), Some(("ns", "ns:pkg", None)));
        assert_eq!(
            classify_specifier("ns:pkg@1.2.3"),
            Some(("ns", "ns:pkg", Some("1.2.3")))
        );
        assert_eq!(
            classify_specifier("lib:foo"),
            Some(("lib", "lib:foo", None))
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

    #[test]
    fn lib_alias_resolves_to_its_package() {
        let dep = only(
            r#"use { X } from "lib:foo"
               with { package: "ns:pkg", registry: "oci://ghcr.io", version: "1.2.3" };"#,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(dep.specifier, "lib:foo");
        assert_eq!(dep.coordinate, "ns:pkg");
        assert_eq!(dep.name, "pkg");
        assert_eq!(dep.version, "1.2.3");
    }

    #[test]
    fn lib_alias_needs_a_package() {
        let err = only(
            r#"use { X } from "lib:foo" with { registry: "oci://ghcr.io", version: "1.2.3" };"#,
            None,
        )
        .unwrap_err();
        assert!(err.contains("needs `with { package"), "{err}");
    }

    #[test]
    fn open_coordinate_rejects_package_aliasing() {
        let err = only(
            r#"use { X } from "ns:pkg@1.2.3" with { package: "other:pkg", registry: "oci://ghcr.io" };"#,
            None,
        )
        .unwrap_err();
        assert!(err.contains("forbidden under an open namespace"), "{err}");
    }
}
