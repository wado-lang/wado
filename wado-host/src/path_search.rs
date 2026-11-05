//! Finding a program on `PATH`, by the rule both `wado <name>` and
//! `core:process` follow.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The directories `PATH` names.
#[must_use]
pub fn search_dirs() -> Vec<PathBuf> {
    dirs_in(&std::env::var_os("PATH").unwrap_or_default())
}

/// An entry that is empty or relative is skipped: the operating system's own
/// search reads an empty entry as the current directory and resolves a
/// relative one against it, so either would let the directory a user stands in
/// decide what runs.
#[must_use]
pub fn dirs_in(path: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .collect()
}

/// The first executable `name` on `PATH`.
#[must_use]
pub fn find_program(name: &str) -> Option<PathBuf> {
    search_dirs().into_iter().find_map(|dir| {
        let path = dir.join(name);
        is_executable(&path).then_some(path)
    })
}

/// Whether this user can run it, not merely whether someone can: a file with
/// an execute bit for another user is one the `PATH` search must walk past.
#[cfg(unix)]
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string for the call's duration.
    unsafe {
        libc::faccessat(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            libc::X_OK,
            libc::AT_EACCESS,
        ) == 0
    }
}

#[cfg(not(unix))]
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    path.is_file()
}
