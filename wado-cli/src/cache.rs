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

/// Resolve the Wado root from `$XDG_CONFIG_HOME/wado/config.toml`'s `root` key
/// and export it as `$WADO_ROOT` when that env var is unset, so the shared
/// [`wado_lsp::host::cache_root`] resolver (and the embedded LSP server) observe
/// the configured root. Config parsing lives here — the single native host — so
/// the wasm-facing crates (`wado-manifest`, `wado-lsp`) never pull a TOML parser
/// or read the filesystem. Precedence: an existing `$WADO_ROOT` wins; else the
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
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
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A per-write temp sibling, unique across processes (pid) and across threads
    // within one process (the counter): a parallel compile fetches the same
    // component from several files at once, so two writers must never share — and
    // rename away — each other's temp file. The rename is the atomic publish step.
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
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

    #[test]
    fn write_atomic_is_concurrency_safe_for_the_same_path() {
        // A parallel compile fetches the same registry component from several
        // files at once, so `write_atomic` runs concurrently on one path. Every
        // writer must succeed and leave the full bytes — no writer may rename
        // another's temp file out from under it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghcr.io/ns/pkg/0.1.0/component.wasm");
        let bytes: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

        const THREADS: usize = 32;
        let barrier = std::sync::Barrier::new(THREADS);
        let results = std::thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        super::write_atomic(&path, &bytes)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });

        for r in &results {
            assert!(r.is_ok(), "a concurrent write_atomic failed: {r:?}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn config_root_value_reads_the_root_string() {
        assert_eq!(
            super::config_root_value("root = \"~/ghq\"\n").as_deref(),
            Some("~/ghq")
        );
        assert_eq!(super::config_root_value("# empty\n"), None);
        assert_eq!(super::config_root_value("root = 42\n"), None);
        assert_eq!(super::config_root_value("not valid ["), None);
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
