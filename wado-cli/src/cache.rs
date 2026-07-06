//! The shared dependency cache: a ghq-style `~/wado/` tree (overridable by
//! `WADO_ROOT`) where fetched registry components live, keyed by their source
//! identity so every project shares one immutable copy instead of re-downloading
//! into each project's `build/`.
//!
//! Layout (CLI-subcommands WEP "Dependency Cache Layout"):
//!
//! ```text
//! {root}/{registry-host}/{namespace}/{name}/{version}/component.wasm
//! ```

use std::path::{Path, PathBuf};

use crate::oci;

/// Basename of a cached prebuilt component inside its version directory.
const COMPONENT_FILE: &str = "component.wasm";

/// The dependency cache root: `$WADO_ROOT`, else `~/wado`.
pub fn root() -> Result<PathBuf, String> {
    if let Some(root) = std::env::var_os("WADO_ROOT").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "cannot locate the home directory; set WADO_ROOT".to_string())?;
    Ok(PathBuf::from(home).join("wado"))
}

/// The cached component path for a registry coordinate at a version, under the
/// shared [`root`].
pub fn component_path(
    registry_url: &str,
    coordinate: &str,
    version: &str,
) -> Result<PathBuf, String> {
    Ok(root()?.join(component_relative(registry_url, coordinate, version)?))
}

/// The cache-root-relative path `{host}/{repository}/{version}/component.wasm`.
/// The host, optional prefix, and `ns:pkg` coordinate map through the same
/// `oci::reference` layout `wado publish` pushes to. Pure (no `root` lookup),
/// so the layout is unit-testable without touching the environment.
fn component_relative(
    registry_url: &str,
    coordinate: &str,
    version: &str,
) -> Result<PathBuf, String> {
    let reference = oci::reference(registry_url, coordinate, version)?;
    Ok(Path::new(reference.registry())
        .join(reference.repository())
        .join(version)
        .join(COMPONENT_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghq_style_layout() {
        let p = component_relative("oci://ghcr.io", "docs:regex", "0.1.2").unwrap();
        assert_eq!(p, Path::new("ghcr.io/docs/regex/0.1.2/component.wasm"));
    }

    #[test]
    fn layout_includes_registry_prefix() {
        let p = component_relative("oci://ghcr.io/acme", "ns:pkg", "1.2.3").unwrap();
        assert_eq!(p, Path::new("ghcr.io/acme/ns/pkg/1.2.3/component.wasm"));
    }

    #[test]
    fn path_hangs_the_layout_off_the_root() {
        // `component_path` is `root().join(component_relative(...))`; the root
        // lookup itself reads process-global env, so verify only the join shape.
        let joined = Path::new("/custom/cache")
            .join(component_relative("oci://ghcr.io", "docs:regex", "0.1.2").unwrap());
        assert_eq!(
            joined,
            Path::new("/custom/cache/ghcr.io/docs/regex/0.1.2/component.wasm")
        );
    }
}
