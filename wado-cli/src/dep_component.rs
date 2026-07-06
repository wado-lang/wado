//! Fetch registry `[dependencies]` as prebuilt Component Model artifacts and
//! map each coordinate to its local `.wasm`, so the compiler resolves
//! `use { X } from "ns:pkg"` across the CM boundary (dependency-management
//! Phase 4). A registry dependency is a standalone component — unlike a path
//! dependency, which compiles into the consumer — so it is imported like a
//! `with { type: "wasm" }` asset, not compiled from source.
//!
//! Components are cached under `<manifest_dir>/build/deps/`; a published version
//! is immutable, so a present cache file is reused without a re-pull.

use std::path::Path;

use wado_manifest::Manifest;

use crate::oci;
use crate::registry::FilesystemProvider;

/// Directory (under the manifest dir) where fetched component dependencies land.
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
        let out_path = deps_dir.join(format!("{name}-{}.wasm", package.version));
        if !out_path.is_file() {
            let reference = oci::reference(registry_url, coordinate, &package.version)
                .map_err(|e| format!("{}: {e}", package.id))?;
            let bytes = oci::pull_component(&reference)
                .await
                .map_err(|e| format!("fetching {coordinate}@{}: {e}", package.version))?;
            std::fs::create_dir_all(&deps_dir)
                .map_err(|e| format!("creating {}: {e}", deps_dir.display()))?;
            std::fs::write(&out_path, &bytes)
                .map_err(|e| format!("writing {}: {e}", out_path.display()))?;
        }
        let abs = out_path
            .canonicalize()
            .unwrap_or(out_path)
            .display()
            .to_string();
        out.push((coordinate.to_string(), abs));
    }
    Ok(out)
}
