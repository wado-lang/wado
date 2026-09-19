//! Subcommands `wado` does not hold, found as `wado-<name>` on `PATH`.
//! See [WEP: External Subcommands](../../docs/wep-2026-09-19-external-subcommands.md).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::args::CliExit;

/// What a subcommand's binary is named with.
const PREFIX: &str = "wado-";

/// One `wado-<name>` that `PATH` offers.
pub struct External {
    pub name: String,
    pub path: PathBuf,
}

/// Every subcommand `PATH` offers, by name. A name that appears twice keeps
/// the earlier directory's file, as running it would.
#[must_use]
pub fn discover() -> Vec<External> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };
            let Some(name) = subcommand_name(file) else {
                continue;
            };
            if is_executable(&path) {
                found.entry(name.to_string()).or_insert(path);
            }
        }
    }
    found
        .into_iter()
        .map(|(name, path)| External { name, path })
        .collect()
}

/// The `wado-<name>` that `wado <name>` would run.
#[must_use]
pub fn find(name: &str) -> Option<PathBuf> {
    if !is_subcommand_name(name) {
        return None;
    }
    search_dirs()
        .into_iter()
        .flat_map(|dir| candidates(&dir, name))
        .find(|path| is_executable(path))
}

/// Run an external subcommand's binary, by the absolute path already
/// resolved. `WADO` names the running binary so the child calls back to the
/// `wado` the user invoked rather than searching for one.
///
/// On Unix this replaces the process, so the exit status and every signal
/// belong to the child. Elsewhere it waits and exits with the child's status.
pub fn run<I, S>(path: &Path, args: I) -> Result<(), CliExit>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(path);
    command.args(args);
    if let Ok(wado) = std::env::current_exe() {
        command.env("WADO", wado);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        let error = command.exec();
        Err(CliExit::error(format!("{}: {error}", path.display())))
    }

    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .map_err(|e| CliExit::error(format!("{}: {e}", path.display())))?;
        Err(CliExit::silent_failure(status.code().unwrap_or(1)))
    }
}

/// The directories a subcommand may come from.
fn search_dirs() -> Vec<PathBuf> {
    dirs_in(&std::env::var_os("PATH").unwrap_or_default())
}

/// An entry that is empty or relative is skipped: the operating system's own
/// search reads an empty entry as the current directory and resolves a
/// relative one against it, so either would let the directory a user stands in
/// decide what `wado foo` runs.
fn dirs_in(path: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .collect()
}

/// A subcommand is spelled the way a builtin is, so a file named `wado-..` or
/// `wado-Run` on `PATH` is not one.
fn is_subcommand_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The subcommand a file name spells, or `None` when it spells none.
fn subcommand_name(file: &str) -> Option<&str> {
    let name = strip_executable_extension(file)?.strip_prefix(PREFIX)?;
    is_subcommand_name(name).then_some(name)
}

#[cfg(unix)]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(format!("{PREFIX}{name}"))]
}

#[cfg(unix)]
fn strip_executable_extension(file: &str) -> Option<&str> {
    Some(file)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Windows decides executability by extension, so `PATHEXT` both names the
/// files to look for and says which of the ones found can run.
#[cfg(not(unix))]
fn executable_extensions() -> Vec<String> {
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(not(unix))]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    executable_extensions()
        .iter()
        .map(|ext| dir.join(format!("{PREFIX}{name}{ext}")))
        .collect()
}

#[cfg(not(unix))]
fn strip_executable_extension(file: &str) -> Option<&str> {
    executable_extensions().iter().find_map(|ext| {
        let stem = file.len().checked_sub(ext.len())?;
        file[stem..]
            .eq_ignore_ascii_case(ext)
            .then(|| &file[..stem])
    })
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_name_spells_a_subcommand_only_in_the_builtin_shape() {
        assert_eq!(
            subcommand_name("wado-run-webgpu"),
            Some("run-webgpu")
        );
        assert_eq!(subcommand_name("wado-x9"), Some("x9"));
        assert_eq!(subcommand_name("wado"), None);
        assert_eq!(subcommand_name("wado-"), None);
        assert_eq!(subcommand_name("wado-.."), None);
        assert_eq!(subcommand_name("wado-Run"), None);
        assert_eq!(subcommand_name("wado-a b"), None);
        assert_eq!(subcommand_name("cargo-run"), None);
    }

    /// The empty entry is the one that reads as the current directory.
    #[test]
    fn the_search_skips_every_entry_that_is_not_absolute() {
        let path = std::env::join_paths(["/usr/bin", "", "rel/bin", "/opt/bin"]).unwrap();
        assert_eq!(
            dirs_in(&path),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/opt/bin")]
        );
    }

    #[test]
    fn a_name_outside_the_builtin_shape_is_never_searched_for() {
        assert!(find("../../bin/sh").is_none());
        assert!(find("").is_none());
    }
}
