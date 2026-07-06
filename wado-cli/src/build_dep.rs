//! Registry resolution for `[build-dependencies]` — the build-time Kiln
//! generator graph.
//!
//! A `[build-dependencies]` entry names a package whose `core:kiln/generator`
//! world is consumed as a generator. Unlike a `[dependencies]` library (pulled
//! from the bare repository), a generator is published to the package's
//! `core-kiln-generator` world sub-path, so both version selection and the
//! integrity digest resolve against that sub-path.
//!
//! These primitives are shared by `wado update` (write `[[build-dependency]]`
//! into the lock), `wado fetch` (pre-pull the generator component), and the Kiln
//! provider (resolve `module: "ns:name"` at compile time).

use std::path::Path;

use indexmap::IndexMap;

use wado_manifest::{DependencySource, LockFile, LockedPackage, Manifest};

use crate::oci;

/// The `core:kiln/generator` world FQ name.
pub const GENERATOR_WORLD_FQ: &str = "core:kiln/generator";

/// The OCI repository sub-path a Kiln generator publishes to (`wado publish`
/// maps the `core:kiln/generator` world to this segment).
pub const GENERATOR_WORLD_SEGMENT: &str = "core-kiln-generator";

/// The parts of a `module:` build-dependency specifier: the `[build-dependencies]`
/// lookup key (`ns:name` or `lib:nick`), an optional pinned `@version`, and an
/// optional `/submodule` path. The coordinate and version never contain `/`, so
/// the first `/` starts the submodule; the `@` split only applies to a segment
/// carrying a `:`.
pub struct SpecParts<'a> {
    pub key: &'a str,
    pub version: Option<&'a str>,
    pub submodule: Option<&'a str>,
}

/// Parse a `module:` specifier into its [`SpecParts`].
#[must_use]
pub fn parse_spec(spec: &str) -> SpecParts<'_> {
    let (head, submodule) = match spec.split_once('/') {
        Some((h, s)) => (h, Some(s)),
        None => (spec, None),
    };
    let (key, version) = match head.split_once('@') {
        Some((k, v)) if k.contains(':') => (k, Some(v)),
        _ => (head, None),
    };
    SpecParts {
        key,
        version,
        submodule,
    }
}

/// The `[build-dependencies]` lookup key for a `module:` specifier.
#[must_use]
pub fn spec_key(spec: &str) -> &str {
    parse_spec(spec).key
}

/// The highest version published at the generator's world sub-path matching
/// `req`. Tags are listed on the sub-path repository — which can be public on
/// GHCR while the bare repository is private.
pub async fn resolve_generator_version(
    registry_url: &str,
    package: &str,
    req: &str,
) -> Result<String, String> {
    let req = semver::VersionReq::parse(req)
        .map_err(|e| format!("invalid version requirement `{req}` for `{package}`: {e}"))?;
    let reference = oci::world_reference(registry_url, package, GENERATOR_WORLD_SEGMENT, "0.0.0")?;
    let tags = oci::list_tags(&reference)
        .await
        .map_err(|e| format!("listing versions of `{package}`: {e}"))?;
    tags.iter()
        .filter_map(|t| crate::registry::parse_version_tag(t))
        .filter(|v| req.matches(v))
        .max()
        .map(|v| v.to_string())
        .ok_or_else(|| format!("no published version of `{package}` matches `{req}`"))
}

/// A resolved build-dependency: the coordinate, its registry URL, and the chosen
/// version. The integrity digest is not read here (it needs an extra registry
/// round-trip and is only wanted by `wado update`); call [`ResolvedBuildDep::integrity`].
pub struct ResolvedBuildDep {
    pub coordinate: String,
    pub registry_url: String,
    pub version: String,
}

impl ResolvedBuildDep {
    /// The OCI reference of the generator component (world sub-path).
    pub fn reference(&self) -> Result<oci::OciReference, String> {
        oci::world_reference(
            &self.registry_url,
            &self.coordinate,
            GENERATOR_WORLD_SEGMENT,
            &self.version,
        )
    }

    /// The sub-path artifact's manifest digest — the lock's `integrity`. One
    /// registry round-trip; called by `wado update` only.
    pub async fn integrity(&self) -> Result<String, String> {
        oci::manifest_digest(&self.reference()?)
            .await
            .map_err(|e| format!("resolving integrity of `{}`: {e}", self.coordinate))
    }

    /// The lock entry for this build-dependency, given its integrity digest.
    #[must_use]
    pub fn locked_package(&self, integrity: String) -> LockedPackage {
        LockedPackage {
            id: format!("registry+{}/{}", self.registry_url, self.coordinate),
            version: self.version.clone(),
            resolved_ref: None,
            integrity: Some(integrity),
            dev: false,
            // Record which world of the package is pinned; the entry path is
            // empty because a registry generator is a prebuilt component, not a
            // source tree.
            world: IndexMap::from([(GENERATOR_WORLD_FQ.to_string(), String::new())]),
            deps: Vec::new(),
        }
    }
}

/// Resolve every registry `[build-dependencies]` entry to its generator
/// coordinate + version. `locked` (`coordinate -> version`, from `wado.lock`)
/// pins the version without a registry listing when present; pass an empty map
/// to always resolve the highest matching version fresh. Path/git/workspace
/// build-deps are skipped (path generators are compiled from source, not pinned,
/// mirroring how path `[dependencies]` are not locked).
pub async fn resolve_build_dependencies(
    manifest: &Manifest,
    locked: &IndexMap<String, String>,
) -> Result<Vec<ResolvedBuildDep>, String> {
    let mut out = Vec::new();
    for (key, dep) in &manifest.build_dependencies {
        let DependencySource::Registry {
            registry,
            package,
            version,
        } = &dep.source
        else {
            continue;
        };
        let registry_url = manifest
            .registries
            .get(registry.as_deref().unwrap_or("default"))
            .cloned()
            .ok_or_else(|| {
                format!("build-dependency `{key}`: no registry in scope (set [registries].default)")
            })?;
        let chosen = match locked.get(package).cloned() {
            Some(v) => v,
            None => resolve_generator_version(&registry_url, package, version).await?,
        };
        out.push(ResolvedBuildDep {
            coordinate: package.clone(),
            registry_url,
            version: chosen,
        });
    }
    Ok(out)
}

/// Pull each resolved build-dependency's generator component into the shared
/// `~/wado/` cache, the location the Kiln provider reads at compile time.
/// Returns the number pulled.
pub async fn fetch_build_dependencies(resolved: &[ResolvedBuildDep]) -> Result<usize, String> {
    for dep in resolved {
        let out = crate::cache::generator_path(&dep.registry_url, &dep.coordinate, &dep.version)?;
        if out.is_file() {
            continue;
        }
        let bytes = oci::pull_component(&dep.reference()?).await.map_err(|e| {
            format!(
                "fetching generator `{}@{}`: {e}",
                dep.coordinate, dep.version
            )
        })?;
        crate::cache::write_atomic(&out, &bytes)
            .map_err(|e| format!("writing {}: {e}", out.display()))?;
    }
    Ok(resolved.len())
}

/// Read the locked generator versions (`coordinate -> version`) from the
/// project's `wado.lock`, if present. Used by the provider to resolve a
/// `module: "ns:name"` spec deterministically without a registry version listing.
#[must_use]
pub fn locked_generator_versions(manifest_root: &Path) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    let Ok(text) = std::fs::read_to_string(manifest_root.join("wado.lock")) else {
        return out;
    };
    let Ok(lock) = text.parse::<LockFile>() else {
        return out;
    };
    for pkg in &lock.build_dependencies {
        // A build-dep lock id is `registry+<url>/<ns>:<pkg>`; the coordinate is
        // the `<ns>:<pkg>` tail after the last `/`.
        if let Some((_, coordinate)) = pkg.id.rsplit_once('/') {
            out.insert(coordinate.to_string(), pkg.version.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_splits_version_and_submodule() {
        let bare = parse_spec("wado-lang:gale");
        assert_eq!(bare.key, "wado-lang:gale");
        assert_eq!(bare.version, None);
        assert_eq!(bare.submodule, None);

        let versioned = parse_spec("wado-lang:gale@0.0.9");
        assert_eq!(versioned.key, "wado-lang:gale");
        assert_eq!(versioned.version, Some("0.0.9"));
        assert_eq!(versioned.submodule, None);

        let sub = parse_spec("example:proto-codegen@1.2/generator");
        assert_eq!(sub.key, "example:proto-codegen");
        assert_eq!(sub.version, Some("1.2"));
        assert_eq!(sub.submodule, Some("generator"));

        let sub_no_ver = parse_spec("lib:gen/sub");
        assert_eq!(sub_no_ver.key, "lib:gen");
        assert_eq!(sub_no_ver.version, None);
        assert_eq!(sub_no_ver.submodule, Some("sub"));
    }

    #[test]
    fn resolved_build_dep_locked_package_pins_integrity_and_world() {
        let dep = ResolvedBuildDep {
            coordinate: "wado-lang:gale".to_string(),
            registry_url: "oci://ghcr.io".to_string(),
            version: "0.0.9".to_string(),
        };
        let pkg = dep.locked_package("sha256:abc".to_string());
        assert_eq!(pkg.id, "registry+oci://ghcr.io/wado-lang:gale");
        assert_eq!(pkg.version, "0.0.9");
        assert_eq!(pkg.integrity.as_deref(), Some("sha256:abc"));
        assert!(pkg.world.contains_key(GENERATOR_WORLD_FQ));
    }

    #[test]
    fn locked_generator_versions_reads_build_dependency_section() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("wado.lock"),
            "version = 1\ndeps-hash = \"sha256:x\"\n\n\
             [[build-dependency]]\n\
             id = \"registry+oci://ghcr.io/wado-lang:gale\"\n\
             version = \"0.0.9\"\n\
             integrity = \"sha256:abc\"\n\
             deps = []\n",
        )
        .unwrap();
        let versions = locked_generator_versions(dir.path());
        assert_eq!(
            versions.get("wado-lang:gale").map(String::as_str),
            Some("0.0.9")
        );
    }
}
