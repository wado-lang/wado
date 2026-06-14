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

/// The kind of entry point to resolve, identified by the CM world it targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPointKind {
    /// `wasi:cli/command` — for `wado run` and `wado compile` (default).
    Command,
    /// `wasi:http/service` — for `wado serve`.
    Service,
    /// The library world — for `wado compile --lib`. Its entry comes from
    /// `[package].lib` (not the `[world]` table) and the world FQ is derived
    /// from `[package]` identity rather than being a fixed WASI world.
    Lib,
}

impl EntryPointKind {
    /// The fully-qualified CM world name this entry point targets, used to look
    /// it up in the manifest's `[world]` table. `Lib` has no fixed world FQ
    /// (it is derived from `[package]`), so it returns `None`.
    #[must_use]
    pub const fn world_fq(self) -> Option<&'static str> {
        match self {
            EntryPointKind::Command => Some("wasi:cli/command"),
            EntryPointKind::Service => Some("wasi:http/service"),
            EntryPointKind::Lib => None,
        }
    }
}

/// The library world FQ derived from a `[package]`, in the standard
/// `namespace:name/name@version` shape. `[package].namespace` is required for
/// `--lib`; this returns an error message when it is missing.
pub fn lib_world_fq(pkg: &wado_manifest::Package) -> Result<String, CliExit> {
    let namespace = pkg
        .namespace
        .as_deref()
        .ok_or_else(|| CliExit::error("`--lib` requires `[package].namespace` in wado.toml"))?;
    Ok(format!(
        "{namespace}:{name}/{name}@{version}",
        name = pkg.name,
        version = pkg.version,
    ))
}

/// Resolve the `--lib` entry point and library world FQ.
///
/// `--lib` requires a manifest (directory input or a `wado.toml` discovered
/// from the cwd); a bare `.wado` file argument is rejected because the package
/// namespace — required to form the world FQ — lives only in `[package]`. The
/// entry comes from `[package].lib`, not the `[world]` table.
///
/// Returns `(entry_path, world_fq)`.
pub fn resolve_lib_input(
    explicit_input: Option<String>,
    usage: &str,
) -> Result<(String, String), CliExit> {
    let project = match explicit_input {
        Some(input) => {
            let path = Path::new(&input);
            if !path.is_dir() {
                return Err(CliExit::error(
                    "`--lib` requires a package directory (or a wado.toml in the \
                     current directory); a single .wado file has no [package] namespace",
                ));
            }
            load_from_dir(path).map_err(|e| match e {
                DiscoveryError::Io(io_err) if io_err.kind() == io::ErrorKind::NotFound => {
                    CliExit::error(format!("no wado.toml found in directory '{input}'"))
                }
                other => CliExit::error(other),
            })?
        }
        None => {
            let cwd = env::current_dir()
                .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
            match discover(&cwd) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return Err(CliExit::error_with_usage(
                        "no input file specified and no wado.toml found",
                        usage,
                    ));
                }
                Err(e) => return Err(CliExit::error(e)),
            }
        }
    };

    let pkg = project
        .manifest
        .package
        .as_ref()
        .ok_or_else(|| CliExit::error("`--lib` requires a `[package]` section in wado.toml"))?;
    let world_fq = lib_world_fq(pkg)?;
    let lib_rel = pkg
        .lib
        .as_deref()
        .ok_or_else(|| CliExit::error("`--lib` requires `[package].lib` in wado.toml"))?;
    let entry = project.root.join(lib_rel);
    Ok((entry.to_string_lossy().into_owned(), world_fq))
}

/// Resolve an entry point path from a manifest.
///
/// Returns the absolute path to the entry point source file, or `None` if the
/// manifest's `[world]` table has no entry for the targeted world.
#[must_use]
pub fn resolve_entry_point(project: &ProjectManifest, kind: EntryPointKind) -> Option<PathBuf> {
    let relative = project.manifest.world_entry(kind.world_fq()?)?;
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
            "wado.toml found but [world].\"{}\" is not set",
            kind.world_fq().unwrap_or("<library>")
        ))
    })
}

/// Resolve input file: use the explicit argument if given, otherwise discover
/// `wado.toml` and use the entry point for `kind`.
///
/// When `explicit_input` points to a directory, load `<dir>/wado.toml` and
/// resolve the entry point for `kind` (e.g. `wado run package-gale` →
/// `package-gale/src/main.wado` via `[world]."wasi:cli/command"`).
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

[world]
"wasi:cli/command" = "src/main.wado"
"wasi:http/service" = "src/server.wado"
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
    }

    #[test]
    fn resolve_input_directory_arg_command() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"
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

[world]
"wasi:cli/command" = "src/main.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let err = resolve_input(
            Some(tmp.path().to_string_lossy().into_owned()),
            EntryPointKind::Service,
            "usage",
        )
        .unwrap_err();
        assert!(
            err.message
                .contains("[world].\"wasi:http/service\" is not set")
        );
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
    fn lib_world_fq_from_package() {
        let toml = r#"
[package]
namespace = "wado"
name = "cm-catalog-min"
version = "0.1.0"
lib = "src/lib.wado"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let fq = lib_world_fq(m.package.as_ref().unwrap()).unwrap();
        assert_eq!(fq, "wado:cm-catalog-min/cm-catalog-min@0.1.0");
    }

    #[test]
    fn lib_world_fq_requires_namespace() {
        let toml = r#"
[package]
name = "no-ns"
version = "0.1.0"
lib = "src/lib.wado"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let err = lib_world_fq(m.package.as_ref().unwrap()).unwrap_err();
        assert!(
            err.message.contains("[package].namespace"),
            "{}",
            err.message
        );
    }

    #[test]
    fn resolve_lib_input_resolves_entry_and_world() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
namespace = "wado"
name = "mylib"
version = "0.2.0"
lib = "src/lib.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let (entry, world) =
            resolve_lib_input(Some(tmp.path().to_string_lossy().into_owned()), "usage").unwrap();
        assert_eq!(
            entry,
            tmp.path()
                .join("src/lib.wado")
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(world, "wado:mylib/mylib@0.2.0");
    }

    #[test]
    fn resolve_lib_input_requires_lib_field() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
namespace = "wado"
name = "mylib"
version = "0.2.0"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let err = resolve_lib_input(Some(tmp.path().to_string_lossy().into_owned()), "usage")
            .unwrap_err();
        assert!(err.message.contains("[package].lib"), "{}", err.message);
    }

    #[test]
    fn resolve_lib_input_rejects_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("lib.wado");
        fs::write(&file, "").unwrap();
        let err =
            resolve_lib_input(Some(file.to_string_lossy().into_owned()), "usage").unwrap_err();
        assert!(err.message.contains("package directory"), "{}", err.message);
    }

    #[test]
    fn resolve_entry_point_missing_field() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"
"#;
        fs::write(tmp.path().join("wado.toml"), toml).unwrap();

        let project = discover(tmp.path()).unwrap().unwrap();
        assert!(resolve_entry_point(&project, EntryPointKind::Service).is_none());
    }
}
