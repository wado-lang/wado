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
//! logic — and the `root` in [`wado_lsp::host::discovery::cache_root`], so the CLI (which
//! fetches) and the language server (which reads offline) resolve one identical
//! path. This module adds the concrete `PathBuf`s.

use crate::build_dep::GENERATOR_WORLD_SEGMENT;
use crate::compile::build_dir;
use crate::user_config::user_config;
use std::path::{Path, PathBuf};
use std::{fs, io};
use wado_lsp::host::discovery::home_dir;

/// The dependency cache root: `$WADO_ROOT`, else `~/wado`. Shares the resolver
/// with the language server so both agree on where the cache lives.
pub fn root() -> Result<PathBuf, String> {
    wado_lsp::host::discovery::cache_root()
        .ok_or_else(|| "cannot locate the home directory; set WADO_ROOT".to_string())
}

/// Hold `name`, a file of the cache's `.locks/`, against every other holder, in
/// this process or another, until the returned file drops.
///
/// # Errors
///
/// The cache's root cannot be found, or the file cannot be opened or held.
pub fn hold(name: &str) -> Result<fs::File, String> {
    let locks = root()?.join(".locks");
    let path = locks.join(name);
    let held = (|| {
        fs::create_dir_all(&locks)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        file.lock()?;
        Ok(file)
    })();
    held.map_err(|e: io::Error| format!("{}: {e}", path.display()))
}

/// Export the user config's `root` as `$WADO_ROOT` when that env var is unset,
/// so the shared [`wado_lsp::host::discovery::cache_root`] resolver (and the
/// embedded LSP server) observe the configured root. Config parsing lives in
/// the single native host, so the crates below it (`wado-manifest`, `wado-lsp`)
/// never pull a TOML parser. Precedence: an existing `$WADO_ROOT` wins; else
/// the config's `root`; else the built-in `~/wado` default applies downstream.
///
/// Must run before any threads start (call it at the top of `main`).
pub fn init_root_from_config() {
    if std::env::var_os("WADO_ROOT").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Some(root) = &user_config().root else {
        return;
    };
    // SAFETY: called at the start of `main`, before the tokio runtime (and any
    // other threads) are created, so there is no concurrent environment access.
    unsafe { std::env::set_var("WADO_ROOT", root) };
}

/// Where derived files outside any package go: `$XDG_CACHE_HOME/wado`, else
/// `~/.cache/wado`. Not the Wado [`root`], which holds dependencies a build
/// needs rather than what any build can recreate.
pub fn user_cache_dir() -> Option<PathBuf> {
    xdg_base_dir("XDG_CACHE_HOME", ".cache").map(|dir| dir.join("wado"))
}

/// The XDG base directory `var` names, else `~/<default>`. A relative value is
/// ignored, as the XDG spec requires: it would name a different directory from
/// every working directory.
pub fn xdg_base_dir(var: &str, default: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
    {
        return Some(dir);
    }
    home_dir().map(|home| home.join(default))
}

/// The directory for the `kind` cache of a file in the package at
/// `package_root`: `build/<kind>/` there, or `<user cache>/<kind>/` for a file
/// in no package, so a script run from a shebang leaves nothing beside itself.
pub fn derived_dir(package_root: Option<&Path>, kind: &str) -> Option<PathBuf> {
    match package_root {
        Some(root) => Some(build_dir(root).join(kind)),
        None => user_cache_dir().map(|dir| dir.join(kind)),
    }
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
        Some(GENERATOR_WORLD_SEGMENT),
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
    use crate::cache::{component_path, generator_path};
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
            component_path("oci://ghcr.io", "docs:regex", "0.1.2").unwrap(),
            Path::new("/custom/cache/ghcr.io/docs/regex/0.1.2/component.wasm")
        );
        assert_eq!(
            generator_path("oci://ghcr.io", "wado-lang:gale", "0.0.9").unwrap(),
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
