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
use std::path::PathBuf;
use wado_lsp::host::discovery::home_dir;

/// The dependency cache root: `$WADO_ROOT`, else `~/wado`. Shares the resolver
/// with the language server so both agree on where the cache lives.
pub fn root() -> Result<PathBuf, String> {
    wado_lsp::host::discovery::cache_root()
        .ok_or_else(|| "cannot locate the home directory; set WADO_ROOT".to_string())
}

/// Resolve the Wado root from `$XDG_CONFIG_HOME/wado/config.toml`'s `root` key
/// and export it as `$WADO_ROOT` when that env var is unset, so the shared
/// [`wado_lsp::host::discovery::cache_root`] resolver (and the embedded LSP server) observe
/// the configured root. Config parsing lives here — the single native host — so
/// the crates below it (`wado-manifest`, `wado-lsp`) never pull
/// a TOML parser. Precedence: an existing `$WADO_ROOT` wins; else the
/// config's `root`; else the built-in `~/wado` default applies downstream.
///
/// Must run before any threads start (call it at the top of `main`); a missing
/// or malformed config is ignored.
pub fn init_root_from_config() {
    if std::env::var_os("WADO_ROOT").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let Some(text) = config_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return;
    };
    let Some(root) = config_root_value(&text).map(|r| expand_tilde(&r)) else {
        return;
    };
    // SAFETY: called at the start of `main`, before the tokio runtime (and any
    // other threads) are created, so there is no concurrent environment access.
    unsafe { std::env::set_var("WADO_ROOT", root) };
}

/// `$XDG_CONFIG_HOME/wado/config.toml`, defaulting to `~/.config/wado/config.toml`.
fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("wado").join("config.toml"));
    }
    home_dir().map(|home| home.join(".config").join("wado").join("config.toml"))
}

/// The `root` string from config TOML text. Pure, so it is unit-testable without
/// touching process-global environment variables. Unknown keys are ignored so
/// the file can carry other settings later; a non-string `root` yields `None`.
fn config_root_value(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct RootConfig {
        root: Option<String>,
    }
    toml::from_str::<RootConfig>(text).ok()?.root
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
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
    use crate::cache::{component_path, config_root_value, generator_path};
    use std::path::Path;

    #[test]
    fn config_root_value_reads_the_root_string() {
        assert_eq!(
            config_root_value("root = \"~/ghq\"\n").as_deref(),
            Some("~/ghq")
        );
        assert_eq!(config_root_value("# empty\n"), None);
        assert_eq!(config_root_value("root = 42\n"), None);
        assert_eq!(config_root_value("not valid ["), None);
    }

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
