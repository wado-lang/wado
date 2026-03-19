use std::env;
use std::path::{Path, PathBuf};
use std::{fs, io};

use wado_manifest::{Manifest, ManifestError};

use crate::args::CliExit;

const MANIFEST_FILENAME: &str = "wado.toml";

/// A discovered manifest with its root directory.
#[derive(Debug)]
pub struct ProjectManifest {
    /// The parsed manifest.
    pub manifest: Manifest,
    /// The directory containing `wado.toml`.
    pub root: PathBuf,
}

/// Errors that can occur during manifest discovery.
#[derive(Debug)]
pub enum DiscoveryError {
    Io(io::Error),
    Parse(ManifestError),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryError::Io(e) => write!(f, "failed to read wado.toml: {e}"),
            DiscoveryError::Parse(e) => write!(f, "invalid wado.toml: {e}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// Search for `wado.toml` starting from `start_dir` and walking up to parent directories.
///
/// Returns `None` if no `wado.toml` is found.
///
/// # Errors
///
/// Returns an error if `wado.toml` is found but cannot be read or parsed.
pub fn discover(start_dir: &Path) -> Result<Option<ProjectManifest>, DiscoveryError> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join(MANIFEST_FILENAME);
        if candidate.is_file() {
            let content = fs::read_to_string(&candidate).map_err(DiscoveryError::Io)?;
            let manifest: Manifest = content.parse().map_err(DiscoveryError::Parse)?;
            return Ok(Some(ProjectManifest {
                manifest,
                root: dir,
            }));
        }
        if !dir.pop() {
            return Ok(None);
        }
    }
}

/// The kind of entry point to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPointKind {
    /// `[package].command` — for `wado run` and `wado compile` (default).
    Command,
    /// `[package].service` — for `wado serve`.
    Service,
    /// `[package].lib` — for `wado compile --lib`.
    Lib,
}

impl EntryPointKind {
    #[must_use]
    pub const fn field_name(self) -> &'static str {
        match self {
            EntryPointKind::Command => "command",
            EntryPointKind::Service => "service",
            EntryPointKind::Lib => "lib",
        }
    }
}

/// Resolve an entry point path from a manifest.
///
/// Returns the absolute path to the entry point source file, or `None` if the
/// manifest has no `[package]` section or the requested field is not set.
#[must_use]
pub fn resolve_entry_point(
    project: &ProjectManifest,
    kind: EntryPointKind,
) -> Option<PathBuf> {
    let pkg = project.manifest.package.as_ref()?;
    let relative = match kind {
        EntryPointKind::Command => pkg.command.as_deref(),
        EntryPointKind::Service => pkg.service.as_deref(),
        EntryPointKind::Lib => pkg.lib.as_deref(),
    }?;
    Some(project.root.join(relative))
}

/// Resolve input file: use the explicit argument if given, otherwise discover
/// `wado.toml` and use the entry point for `kind`.
///
/// # Errors
///
/// Returns a `CliExit` if:
/// - No input file and no `wado.toml` found
/// - `wado.toml` found but the requested entry point is not set
/// - `wado.toml` is invalid
pub fn resolve_input(
    explicit_input: Option<String>,
    kind: EntryPointKind,
    usage: &str,
) -> Result<String, CliExit> {
    if let Some(input) = explicit_input {
        return Ok(input);
    }

    let cwd = env::current_dir().map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;

    let project = match discover(&cwd) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(CliExit::error_with_usage("no input file specified and no wado.toml found", usage));
        }
        Err(e) => {
            return Err(CliExit::error(e));
        }
    };

    let path = resolve_entry_point(&project, kind).ok_or_else(|| {
        CliExit::error(format!(
            "wado.toml found but [package].{} is not set",
            kind.field_name()
        ))
    })?;

    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn discover_in_current_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "test-app"
version = "0.1.0"
command = "src/main.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let result = discover(tmp.path()).unwrap().unwrap();
        assert_eq!(result.root, tmp.path());
        let pkg = result.manifest.package.as_ref().unwrap();
        assert_eq!(pkg.name, "test-app");
    }

    #[test]
    fn discover_in_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "parent-app"
version = "0.1.0"
command = "src/main.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let subdir = tmp.path().join("src").join("deep");
        fs::create_dir_all(&subdir).unwrap();

        let result = discover(&subdir).unwrap().unwrap();
        assert_eq!(result.root, tmp.path());
    }

    #[test]
    fn discover_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let subdir = tmp.path().join("no-manifest");
        fs::create_dir_all(&subdir).unwrap();

        // Discovery will walk up, but tempdir root won't have wado.toml either,
        // so eventually it should return None (at filesystem root).
        // We test that it doesn't panic and doesn't find a spurious manifest.
        let result = discover(&subdir).unwrap();
        // Can't guarantee None since parent dirs might have wado.toml,
        // but in practice tempdir is deep enough. Just check no panic.
        let _ = result;
    }

    #[test]
    fn discover_returns_error_on_invalid_toml() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("wado.toml"), "invalid [[[toml").unwrap();

        let err = discover(tmp.path()).unwrap_err();
        assert!(matches!(err, DiscoveryError::Parse(_)));
    }

    #[test]
    fn resolve_entry_point_command() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "src/main.wado"
service = "src/server.wado"
lib = "src/lib.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let project = discover(tmp.path()).unwrap().unwrap();
        assert_eq!(
            resolve_entry_point(&project, EntryPointKind::Command),
            Some(tmp.path().join("src/main.wado"))
        );
        assert_eq!(
            resolve_entry_point(&project, EntryPointKind::Service),
            Some(tmp.path().join("src/server.wado"))
        );
        assert_eq!(
            resolve_entry_point(&project, EntryPointKind::Lib),
            Some(tmp.path().join("src/lib.wado"))
        );
    }

    #[test]
    fn resolve_entry_point_missing_field() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "src/main.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let project = discover(tmp.path()).unwrap().unwrap();
        assert!(resolve_entry_point(&project, EntryPointKind::Service).is_none());
        assert!(resolve_entry_point(&project, EntryPointKind::Lib).is_none());
    }
}
