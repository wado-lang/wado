//! Where a `[dependencies]` entry belongs, derived from the manifest and an
//! already-parsed `wado.lock`.
//!
//! Pure: coordinates, versions, and cache paths, no disk touched. Reading the
//! lock and checking what is there is `wado_lsp::host::discovery`'s job.

use std::path::{Path, PathBuf};

use crate::cache::registry_cache_relative;
use crate::{DependencySource, LockFile, Manifest, Version, VersionSpecifier, registry_url};

/// A registry `[dependencies]` entry resolved to its exact lock-pinned version
/// and shared-cache location — the single source of truth shared by the LSP
/// (which reads the cache offline) and the CLI (which fetches a cold cache).
#[derive(Debug)]
pub struct RegistryComponentNeed {
    /// Manifest dependency key — the specifier the loader looks up.
    pub name: String,
    /// `oci://…` registry URL the component is pulled from.
    pub registry_url: String,
    /// `ns:pkg` coordinate.
    pub coordinate: String,
    /// Exact version pinned by `wado.lock`.
    pub version: String,
    /// Absolute path the component occupies in the shared cache.
    pub cache_path: PathBuf,
}

/// Every registry `[dependencies]` entry placed against an already-parsed lock.
/// `Err((name, reason))` explains why one cannot go offline (no registry in
/// scope, no lock pin, or no cache root).
pub fn registry_component_needs_locked(
    manifest: &Manifest,
    locked: &std::collections::BTreeMap<String, String>,
    cache_root: Option<&Path>,
) -> Vec<Result<RegistryComponentNeed, (String, String)>> {
    manifest
        .dependencies
        .iter()
        .filter_map(|(name, dep)| match &dep.source {
            DependencySource::Registry {
                registry, package, ..
            } => Some(
                registry_component_need(
                    manifest,
                    name,
                    registry.as_deref(),
                    package,
                    locked,
                    cache_root,
                )
                .map_err(|reason| (name.clone(), reason)),
            ),
            _ => None,
        })
        .collect()
}

pub fn registry_component_need(
    manifest: &Manifest,
    name: &str,
    registry: Option<&str>,
    package: &str,
    locked: &std::collections::BTreeMap<String, String>,
    cache_root: Option<&Path>,
) -> Result<RegistryComponentNeed, String> {
    let registry_url = registry_url(&manifest.registries, registry).ok_or_else(|| {
        let alias = registry.unwrap_or("default");
        format!("no `[registries].{alias}` for {package:?}")
    })?;
    // Match by the full lock id (`registry+<url>/<coordinate>`), not the bare
    // coordinate, so the same package hosted on two registries stays distinct.
    let id = registry_lock_id(registry_url, package);
    let version = locked.get(&id).ok_or_else(|| {
        format!(
            "no version for {package:?}: not pinned by `wado.lock` and not in the cache; \
             run `wado fetch` (or `wado update` to pin it)"
        )
    })?;
    let cache_root =
        cache_root.ok_or_else(|| format!("no cache root for {package:?}; set `WADO_ROOT`"))?;
    let relative = registry_cache_relative(registry_url, package, None, version)
        .ok_or_else(|| format!("cannot place {package:?} in the cache"))?;
    Ok(RegistryComponentNeed {
        name: name.to_string(),
        registry_url: registry_url.clone(),
        coordinate: package.to_string(),
        version: version.clone(),
        cache_path: cache_root.join(relative),
    })
}

/// The lock id a registry dependency pins under. One implementation, so a pin
/// derived from the cache lands where a lock-derived one would.
#[must_use]
pub fn registry_lock_id(registry_url: &str, package: &str) -> String {
    format!("registry+{registry_url}/{package}")
}

/// The newest of `candidates` satisfying `specifier`. A candidate that is not
/// semver is skipped, so a stray cache directory resolves nothing.
pub fn best_matching_version<'a>(
    specifier: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let spec = VersionSpecifier::parse(specifier).ok()?;
    candidates
        .into_iter()
        .filter_map(|c| Version::parse(c).ok())
        .filter(|v| spec.matches(v))
        .max()
        .map(|v| v.to_string())
}

pub fn git_pins(lock: &LockFile) -> std::collections::BTreeMap<String, (String, String)> {
    lock.packages
        .iter()
        .filter_map(|pkg| {
            let resolved = pkg.resolved_ref.clone()?;
            Some((pkg.id.clone(), (pkg.version.clone(), resolved)))
        })
        .collect()
}

/// `lock id -> version` for every registry `[[package]]`. Keyed by the full id
/// so distinct registries never collide.
pub fn registry_pins(lock: &LockFile) -> std::collections::BTreeMap<String, String> {
    lock.packages
        .iter()
        .map(|pkg| (pkg.id.clone(), pkg.version.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{RegistryComponentNeed, best_matching_version, registry_component_need};
    use crate::Manifest;

    fn manifest_with_registry_dep() -> Manifest {
        "[package]\nname=\"app\"\nversion=\"0.1.0\"\n\n\
         [registries]\ndefault=\"oci://ghcr.io\"\n\n\
         [dependencies]\n\"wado-lang:cm-catalog\" = { version = \"^0.1\" }\n"
            .parse()
            .unwrap()
    }

    fn locked(version: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(
            "registry+oci://ghcr.io/wado-lang:cm-catalog".to_string(),
            version.to_string(),
        )])
    }

    fn need(
        locked: &BTreeMap<String, String>,
        cache_root: Option<&Path>,
    ) -> Result<RegistryComponentNeed, String> {
        registry_component_need(
            &manifest_with_registry_dep(),
            "wado-lang:cm-catalog",
            None,
            "wado-lang:cm-catalog",
            locked,
            cache_root,
        )
    }

    #[test]
    fn need_uses_the_lock_version_and_ghq_layout() {
        let n = need(&locked("0.1.0"), Some(Path::new("/cache"))).unwrap();
        assert_eq!(n.name, "wado-lang:cm-catalog");
        assert_eq!(n.coordinate, "wado-lang:cm-catalog");
        assert_eq!(n.version, "0.1.0");
        assert_eq!(n.registry_url, "oci://ghcr.io");
        assert_eq!(
            n.cache_path,
            Path::new("/cache/ghcr.io/wado-lang/cm-catalog/0.1.0/component.wasm")
        );
    }

    #[test]
    fn need_matches_by_full_lock_id_not_bare_coordinate() {
        // A different registry hosting the same coordinate must not match.
        let other = BTreeMap::from([(
            "registry+oci://other.io/wado-lang:cm-catalog".to_string(),
            "9.9.9".to_string(),
        )]);
        let err = need(&other, Some(Path::new("/cache"))).unwrap_err();
        assert!(err.contains("wado update"), "{err}");
    }

    #[test]
    fn need_without_lock_pin_asks_for_update() {
        let err = need(&BTreeMap::new(), Some(Path::new("/cache"))).unwrap_err();
        assert!(err.contains("wado update"), "{err}");
    }

    #[test]
    fn best_matching_version_takes_the_newest_match() {
        assert_eq!(
            best_matching_version("^0.1", ["0.1.0", "0.1.9", "0.2.0"]),
            Some("0.1.9".to_string())
        );
        assert_eq!(best_matching_version("^0.1", ["0.0.29"]), None);
    }

    #[test]
    fn best_matching_version_skips_non_semver_candidates() {
        // A world segment (`core-kiln-generator`) sits beside the version
        // directories, so a cache listing carries names that are not versions.
        assert_eq!(
            best_matching_version("^1.0", ["core-kiln-generator", "1.2.0"]),
            Some("1.2.0".to_string())
        );
    }

    #[test]
    fn need_without_cache_root_asks_for_wado_root() {
        let err = need(&locked("0.1.0"), None).unwrap_err();
        assert!(err.contains("WADO_ROOT"), "{err}");
    }
}
