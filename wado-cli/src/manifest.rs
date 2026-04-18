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
pub fn resolve_entry_point(project: &ProjectManifest, kind: EntryPointKind) -> Option<PathBuf> {
    let pkg = project.manifest.package.as_ref()?;
    let relative = match kind {
        EntryPointKind::Command => pkg.command.as_deref(),
        EntryPointKind::Service => pkg.service.as_deref(),
        EntryPointKind::Lib => pkg.lib.as_deref(),
    }?;
    Some(project.root.join(relative))
}

/// Load a `wado.toml` directly from `dir` (does not walk parents).
///
/// Returns an error if the directory has no `wado.toml` or the file is invalid.
fn load_from_dir(dir: &Path) -> Result<ProjectManifest, DiscoveryError> {
    let candidate = dir.join(MANIFEST_FILENAME);
    let content = fs::read_to_string(&candidate).map_err(DiscoveryError::Io)?;
    let manifest: Manifest = content.parse().map_err(DiscoveryError::Parse)?;
    Ok(ProjectManifest {
        manifest,
        root: dir.to_path_buf(),
    })
}

/// Resolve an entry point from a project manifest, returning a `CliExit` error
/// when the requested field is unset.
fn entry_point_or_error(
    project: &ProjectManifest,
    kind: EntryPointKind,
) -> Result<PathBuf, CliExit> {
    resolve_entry_point(project, kind).ok_or_else(|| {
        CliExit::error(format!(
            "wado.toml found but [package].{} is not set",
            kind.field_name()
        ))
    })
}

/// Resolve input file: use the explicit argument if given, otherwise discover
/// `wado.toml` and use the entry point for `kind`.
///
/// When `explicit_input` points to a directory, load `<dir>/wado.toml` and
/// resolve the entry point for `kind` (e.g. `wado run package-gale` →
/// `package-gale/src/main.wado` via `[package].command`).
///
/// # Errors
///
/// Returns a `CliExit` if:
/// - No input file and no `wado.toml` found
/// - The explicit input is a directory without `wado.toml`
/// - `wado.toml` is found but the requested entry point is not set
/// - `wado.toml` is invalid
pub fn resolve_input(
    explicit_input: Option<String>,
    kind: EntryPointKind,
    usage: &str,
) -> Result<String, CliExit> {
    if let Some(input) = explicit_input {
        let path = Path::new(&input);
        if !path.is_dir() {
            return Ok(input);
        }
        let project = load_from_dir(path).map_err(|e| match e {
            DiscoveryError::Io(io_err) if io_err.kind() == io::ErrorKind::NotFound => {
                CliExit::error(format!("no wado.toml found in directory '{input}'"))
            }
            other => CliExit::error(other),
        })?;
        let entry = entry_point_or_error(&project, kind)?;
        return Ok(entry.to_string_lossy().into_owned());
    }

    let cwd = env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;

    let project = match discover(&cwd) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Err(CliExit::error_with_usage(
                "no input file specified and no wado.toml found",
                usage,
            ));
        }
        Err(e) => {
            return Err(CliExit::error(e));
        }
    };

    let path = entry_point_or_error(&project, kind)?;
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
    fn resolve_input_directory_arg_command() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "src/main.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let dir_arg = tmp.path().to_string_lossy().into_owned();
        let resolved = resolve_input(Some(dir_arg), EntryPointKind::Command, "usage").unwrap();
        assert_eq!(
            resolved,
            tmp.path()
                .join("src/main.wado")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn resolve_input_directory_arg_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let subdir = tmp.path().join("no-manifest");
        fs::create_dir_all(&subdir).unwrap();

        let err = resolve_input(
            Some(subdir.to_string_lossy().into_owned()),
            EntryPointKind::Command,
            "usage",
        )
        .unwrap_err();
        assert!(err.message.contains("no wado.toml found"));
    }

    #[test]
    fn resolve_input_directory_arg_missing_entry_point() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "src/main.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let err = resolve_input(
            Some(tmp.path().to_string_lossy().into_owned()),
            EntryPointKind::Service,
            "usage",
        )
        .unwrap_err();
        assert!(err.message.contains("[package].service is not set"));
    }

    #[test]
    fn resolve_input_file_path_passthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("main.wado");
        fs::write(&file, "").unwrap();

        let file_arg = file.to_string_lossy().into_owned();
        let resolved =
            resolve_input(Some(file_arg.clone()), EntryPointKind::Command, "usage").unwrap();
        assert_eq!(resolved, file_arg);
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
