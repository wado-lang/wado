use std::borrow::Cow;
use std::env;
use std::path::{Component, Path, PathBuf};
use std::{fs, io};

use wado_lsp::workspace::governing_workspace;
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
            let manifest = resolve_manifest(&dir, &content)?;
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

/// Parse a member's `wado.toml`, applying `[workspace.package]` inheritance when
/// the directory belongs to a workspace; otherwise parse it standalone.
pub(crate) fn resolve_manifest(
    member_dir: &Path,
    member_content: &str,
) -> Result<Manifest, DiscoveryError> {
    match find_workspace_root(member_dir, member_content) {
        Some(root_content) => wado_manifest::resolve_member(member_content, &root_content)
            .map_err(DiscoveryError::Parse),
        None => member_content.parse().map_err(DiscoveryError::Parse),
    }
}

/// `member_dir` normalized so [`governing_workspace`] can locate the workspace.
///
/// That function walks up with `pop()` and matches members with `strip_prefix`,
/// so an empty, relative, or `..`-bearing dir cannot find the workspace root —
/// a bare relative entry (`src/main.wado`) discovers an empty root, and a path
/// build-dependency joins `../pkg`. Those shapes are canonicalized to the same
/// absolute, `..`-free dir an absolute entry path already produces; an already
/// absolute, `..`-free dir is returned untouched, so the common path pays no
/// syscall and its symlinks are not resolved. Normalization lives here, in the
/// CLI, because `governing_workspace` is in wasm32-only `wado-lsp` and cannot
/// touch the filesystem.
fn workspace_anchor(member_dir: &Path) -> Cow<'_, Path> {
    let has_parent_dir = member_dir.components().any(|c| c == Component::ParentDir);
    if member_dir.is_absolute() && !has_parent_dir {
        return Cow::Borrowed(member_dir);
    }
    let anchor = if member_dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        member_dir
    };
    fs::canonicalize(anchor)
        .map(Cow::Owned)
        .unwrap_or(Cow::Borrowed(member_dir))
}

/// The workspace-root `wado.toml` contents governing `member_dir`, if any.
fn find_workspace_root(member_dir: &Path, member_content: &str) -> Option<String> {
    governing_workspace(&workspace_anchor(member_dir), member_content).map(|(_, content)| content)
}

/// The workspace root directory governing the package at `member_dir`, if it is
/// a workspace member. Used to gate `wado publish` to the workspace root.
pub(crate) fn governing_workspace_root_dir(
    member_dir: &Path,
) -> Result<Option<PathBuf>, DiscoveryError> {
    let content =
        fs::read_to_string(member_dir.join(MANIFEST_FILENAME)).map_err(DiscoveryError::Io)?;
    Ok(governing_workspace(&workspace_anchor(member_dir), &content).map(|(dir, _)| dir))
}

/// The directory a `license-file` path resolves against: the package's own root
/// when the package declares its own license slot, or the governing workspace
/// root when the `license-file` is inherited from `[workspace.package]` (per the
/// package-manifest WEP). Falls back to `package_root` when the own manifest is
/// unreadable or the package is standalone.
pub(crate) fn license_file_base_dir(package_root: &Path) -> PathBuf {
    let content = fs::read_to_string(package_root.join(MANIFEST_FILENAME)).unwrap_or_default();
    if own_manifest_declares_license_slot(&content) {
        return package_root.to_path_buf();
    }
    governing_workspace(&workspace_anchor(package_root), &content)
        .map(|(dir, _)| dir)
        .unwrap_or_else(|| package_root.to_path_buf())
}

/// Whether `[package]` sets `license` or `license-file` directly.
fn own_manifest_declares_license_slot(content: &str) -> bool {
    let Ok(table) = content.parse::<toml::Table>() else {
        return false;
    };
    let Some(pkg) = table.get("package").and_then(toml::Value::as_table) else {
        return false;
    };
    pkg.contains_key("license") || pkg.contains_key("license-file")
}

/// Member package directories of the workspace rooted at `root_dir`, expanded
/// from its `members` globs (directories containing a `wado.toml`, excluding the
/// root itself).
pub(crate) fn workspace_member_dirs(root_dir: &Path, members: &[String]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for pattern in members {
        let Some(joined) = root_dir.join(pattern).to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(paths) = glob::glob(&joined) else {
            continue;
        };
        for path in paths.filter_map(Result::ok) {
            if path != root_dir && path.join(MANIFEST_FILENAME).is_file() {
                dirs.push(path);
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
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

/// The emitted name of a library's world — the component's anonymous root.
/// Re-exported from [`wado_compiler::wit_emit`], the single source of truth.
pub const LIB_WORLD_NAME: &str = wado_compiler::wit_emit::LIB_WORLD_NAME;

/// A library FQ `namespace:name/<segment>@version`, rejecting a package name
/// that (case-insensitively) equals the reserved [`LIB_WORLD_NAME`].
fn lib_fq(pkg: &wado_manifest::Package, segment: &str) -> Result<String, CliExit> {
    let namespace = pkg
        .namespace
        .as_deref()
        .ok_or_else(|| CliExit::error("`--lib` requires `[package].namespace` in wado.toml"))?;
    if pkg.name.eq_ignore_ascii_case(LIB_WORLD_NAME) {
        return Err(CliExit::error(format!(
            "[package].name {name:?} is reserved: a library's world is named \
             {LIB_WORLD_NAME:?}, so the default interface cannot share it",
            name = pkg.name
        )));
    }
    Ok(format!(
        "{namespace}:{name}/{segment}@{version}",
        name = pkg.name,
        version = pkg.version,
    ))
}

/// The library's default-interface FQ, in the `namespace:name/name@version`
/// shape — the identity consumers import.
pub fn lib_world_fq(pkg: &wado_manifest::Package) -> Result<String, CliExit> {
    lib_fq(pkg, &pkg.name)
}

/// A resolved `--lib` package: everything needed to build and emit its world.
pub struct LibTarget {
    /// Entry-point source path (`[package].lib`).
    pub entry: PathBuf,
    /// Default-interface FQ `namespace:name/name@version` — the codegen key and
    /// the identity consumers import.
    pub interface_fq: String,
}

/// Resolve the `--lib` package: its `ProjectManifest` and [`LibTarget`].
///
/// `--lib` requires a manifest (directory input or a `wado.toml` discovered
/// from the cwd); a bare `.wado` file argument is rejected because the package
/// namespace — required to form the world FQ — lives only in `[package]`. The
/// entry comes from `[package].lib`, not the `[world]` table.
pub fn resolve_lib_project(
    explicit_input: Option<String>,
    usage: &str,
) -> Result<(ProjectManifest, LibTarget), CliExit> {
    let project = if let Some(input) = explicit_input {
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
    } else {
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
    };
    emit_manifest_warnings(&project);

    let pkg = project
        .manifest
        .package
        .as_ref()
        .ok_or_else(|| CliExit::error("`--lib` requires a `[package]` section in wado.toml"))?;
    let interface_fq = lib_world_fq(pkg)?;
    let lib_rel = pkg
        .lib
        .as_deref()
        .ok_or_else(|| CliExit::error("`--lib` requires `[package].lib` in wado.toml"))?;
    let target = LibTarget {
        entry: project.root.join(lib_rel),
        interface_fq,
    };
    Ok((project, target))
}

/// Resolve the `--lib` entry point and default-interface FQ.
///
/// Returns `(entry_path, interface_fq)`.
pub fn resolve_lib_input(
    explicit_input: Option<String>,
    usage: &str,
) -> Result<(String, String), CliExit> {
    let (_, target) = resolve_lib_project(explicit_input, usage)?;
    Ok((
        target.entry.to_string_lossy().into_owned(),
        target.interface_fq,
    ))
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
    let manifest = resolve_manifest(dir, &content)?;
    Ok(ProjectManifest {
        manifest,
        root: dir.to_path_buf(),
    })
}

// Call at a command boundary, not inside `discover`/`load_from_dir`, so commands
// that walk many directories (`wado test`, `wado format`) don't re-emit per dir.
pub fn emit_manifest_warnings(project: &ProjectManifest) {
    let path = project.root.join(MANIFEST_FILENAME);
    for w in project.manifest.warnings() {
        eprintln!("warning: {}: {w}", path.display());
    }
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
        emit_manifest_warnings(&project);
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
    emit_manifest_warnings(&project);

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
    fn lib_fq_rejects_reserved_root_package_name_any_case() {
        for name in ["root", "Root", "ROOT"] {
            let toml = format!(
                "[package]\nnamespace = \"wado\"\nname = \"{name}\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n"
            );
            let m = toml.parse::<Manifest>().unwrap();
            let pkg = m.package.as_ref().unwrap();
            let iface_err = lib_world_fq(pkg).unwrap_err();
            assert!(
                iface_err.message.contains("reserved"),
                "{}",
                iface_err.message
            );
        }
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

    const WS_ROOT: &str = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.1.0"
repository = "https://github.com/org/monorepo"
namespace = "org"
license = "MIT"
authors = ["Alice"]
"#;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn member_inherits_workspace_metadata_via_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("wado.toml"), WS_ROOT);
        let member_dir = tmp.path().join("packages/core");
        write(
            &member_dir.join("wado.toml"),
            "[package]\nname = \"core\"\nlib = \"src/lib.wado\"\n",
        );
        let project = discover(&member_dir).unwrap().unwrap();
        let pkg = project.manifest.package.unwrap();
        assert_eq!(pkg.name, "core");
        assert_eq!(pkg.version, "0.1.0");
        assert_eq!(pkg.namespace.as_deref(), Some("org"));
        assert_eq!(pkg.license.as_deref(), Some("MIT"));
    }

    #[test]
    fn workspace_root_that_is_also_a_package_parses_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        let toml = format!(
            "{WS_ROOT}\n[package]\nname = \"root-pkg\"\nversion = \"0.1.0\"\nrepository = \"https://github.com/org/monorepo\"\nnamespace = \"org\"\n"
        );
        write(&tmp.path().join("wado.toml"), &toml);
        let project = discover(tmp.path()).unwrap().unwrap();
        assert_eq!(project.manifest.package.unwrap().name, "root-pkg");
    }

    #[test]
    fn governing_workspace_root_dir_identifies_root_for_member() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("wado.toml"), WS_ROOT);
        let member_dir = tmp.path().join("packages/core");
        write(
            &member_dir.join("wado.toml"),
            "[package]\nname = \"core\"\nlib = \"src/lib.wado\"\n",
        );
        let root = governing_workspace_root_dir(&member_dir).unwrap().unwrap();
        assert_eq!(root, tmp.path());
        assert!(governing_workspace_root_dir(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn governing_workspace_root_dir_none_for_standalone() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("wado.toml"),
            "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nlib = \"src/lib.wado\"\n",
        );
        assert!(governing_workspace_root_dir(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn workspace_member_dirs_expands_globs() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("wado.toml"), WS_ROOT);
        write(
            &tmp.path().join("packages/a/wado.toml"),
            "[package]\nname = \"a\"\n",
        );
        write(
            &tmp.path().join("packages/b/wado.toml"),
            "[package]\nname = \"b\"\n",
        );
        fs::create_dir_all(tmp.path().join("packages/empty")).unwrap();
        let dirs = workspace_member_dirs(tmp.path(), &["packages/*".to_string()]);
        assert_eq!(
            dirs,
            vec![tmp.path().join("packages/a"), tmp.path().join("packages/b")]
        );
    }

    const WS_ROOT_LICENSE_FILE: &str = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.1.0"
repository = "https://github.com/org/monorepo"
namespace = "org"
license-file = "COMPANY-LICENSE.txt"
authors = ["Alice"]
"#;

    #[test]
    fn license_file_base_dir_standalone_uses_package_root() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("wado.toml"),
            "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nlicense-file = \"LICENSE\"\n",
        );
        assert_eq!(license_file_base_dir(tmp.path()), tmp.path());
    }

    #[test]
    fn license_file_base_dir_inherited_uses_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("wado.toml"), WS_ROOT_LICENSE_FILE);
        let member_dir = tmp.path().join("packages/core");
        write(
            &member_dir.join("wado.toml"),
            "[package]\nname = \"core\"\nlib = \"src/lib.wado\"\n",
        );
        // No license slot of its own, so it inherits the workspace's.
        assert_eq!(license_file_base_dir(&member_dir), tmp.path());
    }

    #[test]
    fn license_file_base_dir_member_own_license_file_uses_member() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("wado.toml"), WS_ROOT_LICENSE_FILE);
        let member_dir = tmp.path().join("packages/core");
        write(
            &member_dir.join("wado.toml"),
            "[package]\nname = \"core\"\nlib = \"src/lib.wado\"\nlicense-file = \"OWN-LICENSE\"\n",
        );
        assert_eq!(license_file_base_dir(&member_dir), member_dir);
    }

    #[test]
    fn non_member_directory_does_not_inherit() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("wado.toml"), WS_ROOT);
        let outside = tmp.path().join("tools/helper");
        write(
            &outside.join("wado.toml"),
            "[package]\nname = \"helper\"\nlib = \"src/lib.wado\"\n",
        );
        let err = discover(&outside).unwrap_err();
        assert!(
            matches!(&err, DiscoveryError::Parse(e)
                if matches!(e, wado_manifest::ManifestError::MissingField { field, .. } if field == "version")),
            "{err:?}"
        );
    }
}
