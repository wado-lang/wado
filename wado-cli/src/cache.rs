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
//! logic — and the `root` in [`wado_lsp::host::cache_root`], so the CLI (which
//! fetches) and the language server (which reads offline) resolve one identical
//! path. This module adds the concrete `PathBuf`s and the atomic writer.

use std::io;
use std::path::{Path, PathBuf};

/// The dependency cache root: `$WADO_ROOT`, else `~/wado`. Shares the resolver
/// with the language server so both agree on where the cache lives.
pub fn root() -> Result<PathBuf, String> {
    wado_lsp::host::cache_root()
        .ok_or_else(|| "cannot locate the home directory; set WADO_ROOT".to_string())
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

/// Write `bytes` to `path` atomically: create the parent dir, write a sibling
/// temp file, then rename into place. A crash or a concurrent writer can only
/// leave a stray temp file, never a half-written `component.wasm` that the
/// `is_file()` cache check would then trust forever.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // The pid keeps concurrent writers (different processes) from clobbering one
    // another's temp file; the rename is the single atomic publish step.
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
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
