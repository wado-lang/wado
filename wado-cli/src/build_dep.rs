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

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use sha2::{Digest, Sha256};

use wado_manifest::{DependencySource, LockFile, LockedPackage, Manifest};

use crate::oci;

/// The `core:kiln/generator` world FQ name.
pub const GENERATOR_WORLD_FQ: &str = "core:kiln/generator";

/// The OCI repository sub-path a Kiln generator publishes to (`wado publish`
/// maps the `core:kiln/generator` world to this segment).
pub const GENERATOR_WORLD_SEGMENT: &str = "core-kiln-generator";

/// Stable cache id for a generator coordinate. A published version is immutable,
/// so keying on the coordinate is enough for the on-disk component cache
/// (`build/kiln/generators/<id>.wasm`); `wado fetch` and the provider derive the
/// same id, so a pre-fetched component is a warm cache hit at compile time.
#[must_use]
pub fn generator_stable_id(coordinate: &str) -> String {
    let digest = Sha256::digest(coordinate.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("spec-{hex}")
}

/// The on-disk cache path for a generator coordinate's pulled component.
#[must_use]
pub fn generator_cache_path(manifest_root: &Path, coordinate: &str) -> PathBuf {
    manifest_root
        .join(crate::kiln_provider::CACHE_DIR)
        .join(format!("{}.wasm", generator_stable_id(coordinate)))
}

/// Parse an OCI image tag into a [`semver::Version`], stripping an optional
/// leading letter prefix (`v1.2.3`) as the registry resolver does. Non-semver
/// tags (`latest`, …) yield `None`.
fn parse_version_tag(tag: &str) -> Option<semver::Version> {
    if let Ok(version) = semver::Version::parse(tag) {
        return Some(version);
    }
    let rest = tag.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    (rest.len() < tag.len())
        .then(|| semver::Version::parse(rest).ok())
        .flatten()
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
        .filter_map(|t| parse_version_tag(t))
        .filter(|v| req.matches(v))
        .max()
        .map(|v| v.to_string())
        .ok_or_else(|| format!("no published version of `{package}` matches `{req}`"))
}

/// A resolved build-dependency: the coordinate, its registry URL, chosen
/// version, and the sub-path artifact's integrity digest.
pub struct ResolvedBuildDep {
    pub coordinate: String,
    pub registry_url: String,
    pub version: String,
    pub integrity: String,
}

impl ResolvedBuildDep {
    /// The OCI reference of the generator component (world sub-path).
    fn reference(&self) -> Result<oci::OciReference, String> {
        oci::world_reference(
            &self.registry_url,
            &self.coordinate,
            GENERATOR_WORLD_SEGMENT,
            &self.version,
        )
    }

    /// The lock entry for this build-dependency.
    #[must_use]
    pub fn locked_package(&self) -> LockedPackage {
        LockedPackage {
            id: format!("registry+{}/{}", self.registry_url, self.coordinate),
            version: self.version.clone(),
            resolved_ref: None,
            integrity: Some(self.integrity.clone()),
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
/// component: pick the highest published version at the world sub-path and read
/// the sub-path artifact's manifest digest as the integrity. Path/git/workspace
/// build-deps are skipped (path generators are compiled from source, not pinned,
/// mirroring how path `[dependencies]` are not locked).
pub async fn resolve_build_dependencies(
    manifest: &Manifest,
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
        let chosen = resolve_generator_version(&registry_url, package, version).await?;
        let resolved = ResolvedBuildDep {
            coordinate: package.clone(),
            registry_url,
            version: chosen,
            integrity: String::new(),
        };
        let integrity = oci::manifest_digest(&resolved.reference()?)
            .await
            .map_err(|e| format!("resolving integrity of `{package}`: {e}"))?;
        out.push(ResolvedBuildDep {
            integrity,
            ..resolved
        });
    }
    Ok(out)
}

/// Pull each resolved build-dependency's generator component into the on-disk
/// generator cache (`build/kiln/generators/`), the location the Kiln provider
/// reads at compile time. Returns the number pulled.
pub async fn fetch_build_dependencies(
    resolved: &[ResolvedBuildDep],
    manifest_root: &Path,
) -> Result<usize, String> {
    for dep in resolved {
        let bytes = oci::pull_component(&dep.reference()?).await.map_err(|e| {
            format!(
                "fetching generator `{}@{}`: {e}",
                dep.coordinate, dep.version
            )
        })?;
        let out = generator_cache_path(manifest_root, &dep.coordinate);
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        }
        std::fs::write(&out, &bytes).map_err(|e| format!("writing {}: {e}", out.display()))?;
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
    fn stable_id_is_deterministic_and_coordinate_scoped() {
        let a = generator_stable_id("wado-lang:gale");
        assert_eq!(a, generator_stable_id("wado-lang:gale"));
        assert_ne!(a, generator_stable_id("wado-lang:jade"));
        assert!(a.starts_with("spec-"), "{a}");
    }

    #[test]
    fn parse_version_tag_reads_semver_and_prefixed() {
        assert_eq!(
            parse_version_tag("0.0.9"),
            semver::Version::parse("0.0.9").ok()
        );
        assert_eq!(
            parse_version_tag("v1.2.3"),
            semver::Version::parse("1.2.3").ok()
        );
        assert_eq!(parse_version_tag("latest"), None);
    }

    #[test]
    fn resolved_build_dep_locked_package_pins_integrity_and_world() {
        let dep = ResolvedBuildDep {
            coordinate: "wado-lang:gale".to_string(),
            registry_url: "oci://ghcr.io".to_string(),
            version: "0.0.9".to_string(),
            integrity: "sha256:abc".to_string(),
        };
        let pkg = dep.locked_package();
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
