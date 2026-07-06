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
//!
//! A Kiln generator caches under its package's `core-kiln-generator` world
//! sub-path (the location it publishes to), so a library component and a
//! generator of the same package share the tree without colliding.
//!
//! The path *layout* lives in [`wado_manifest::cache`] — pure, portable string
//! logic the language server also reads offline. This module adds the `root`
//! (which needs the environment) and the concrete `PathBuf`s.

use std::path::PathBuf;

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
    relative_under_root(registry_url, coordinate, None, version)
}

/// The cached path for a Kiln generator component. A generator publishes to its
/// package's `core-kiln-generator` world sub-path, so it caches under that same
/// sub-path — sharing the tree with the package's library component but never
/// colliding with it.
pub fn generator_path(
    registry_url: &str,
    coordinate: &str,
    version: &str,
) -> Result<PathBuf, String> {
    relative_under_root(
        registry_url,
        coordinate,
        Some(crate::build_dep::GENERATOR_WORLD_SEGMENT),
        version,
    )
}

fn relative_under_root(
    registry_url: &str,
    coordinate: &str,
    world_segment: Option<&str>,
    version: &str,
) -> Result<PathBuf, String> {
    let relative = wado_manifest::cache::registry_cache_relative(
        registry_url,
        coordinate,
        world_segment,
        version,
    )
    .ok_or_else(|| format!("cannot cache {coordinate}@{version} from {registry_url:?}"))?;
    Ok(root()?.join(relative))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    // SAFETY: single-threaded test, env restored before returning.
    #[test]
    fn paths_hang_the_shared_layout_off_wado_root() {
        // The layout itself is covered in `wado_manifest::cache`; here just pin
        // that `root()` (env) composes with it. Serialised via one test to avoid
        // process-global env races.
        let prev = std::env::var_os("WADO_ROOT");
        unsafe { std::env::set_var("WADO_ROOT", "/custom/cache") };

        assert_eq!(
            super::component_path("oci://ghcr.io", "docs:regex", "0.1.2").unwrap(),
            Path::new("/custom/cache/ghcr.io/docs/regex/0.1.2/component.wasm")
        );
        assert_eq!(
            super::generator_path("oci://ghcr.io", "wado-lang:gale", "0.0.9").unwrap(),
            Path::new(
                "/custom/cache/ghcr.io/wado-lang/gale/core-kiln-generator/0.0.9/component.wasm"
            )
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("WADO_ROOT", v) },
            None => unsafe { std::env::remove_var("WADO_ROOT") },
        }
    }
}
