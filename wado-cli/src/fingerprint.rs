//! What a cached outcome is known by: the compiler that produced it, and the
//! files it read. Shared by the `core:eval` outcome cache and the `wado test`
//! result cache.

use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};
use wado_lsp::host::read_prefix;

use crate::kiln_provider::hex32;

/// What every outcome depends on beyond its own inputs: the running `wado`
/// binary, which links the compiler and wasmtime, and the stdlib it compiles
/// against, which a dev build reads from disk.
///
/// The binary is known by its size and modification time, as ccache knows a
/// compiler by default. Hashing its contents costs seconds for a dev build,
/// and every rebuild changes both anyway.
///
/// A rebuild beneath a long `wado test` replaces the file at the binary's
/// path, which then describes another compiler, so the digest is read when the
/// run starts, and on Linux through the running image itself.
pub(crate) fn compiler_digest() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(binary_digest());
    hash_dev_stdlib(&mut hasher);
    hasher.finalize().into()
}

/// [`compiler_digest`] without the stdlib, for a cache that records the stdlib
/// modules each compile took as inputs of their own.
pub(crate) fn binary_digest() -> [u8; 32] {
    let exe = running_exe();
    let stat = std::fs::metadata(&exe)
        .unwrap_or_else(|e| panic!("reading the running `wado` binary {}: {e}", exe.display()));
    let modified = stat
        .modified()
        .expect("the platform reports a modification time");
    let nanos_since_epoch = match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => i128::try_from(after.as_nanos()),
        Err(before) => i128::try_from(before.duration().as_nanos()).map(|nanos| -nanos),
    }
    .expect("a modification time within i128 nanoseconds of 1970");
    let mut hasher = Sha256::new();
    hasher.update(stat.len().to_le_bytes());
    hasher.update(nanos_since_epoch.to_le_bytes());
    hasher.finalize().into()
}

#[cfg(target_os = "linux")]
fn running_exe() -> PathBuf {
    PathBuf::from("/proc/self/exe")
}

#[cfg(not(target_os = "linux"))]
fn running_exe() -> PathBuf {
    std::env::current_exe().expect("locating the running `wado` binary")
}

#[cfg(debug_assertions)]
fn hash_dev_stdlib(hasher: &mut Sha256) {
    wado_lsp::host::install_dev_stdlib();
    for (file, source) in wado_compiler::stdlib::installed_dev_stdlib() {
        hash_field(hasher, file.as_bytes());
        hash_field(hasher, source.as_bytes());
    }
}

/// A release build embeds the stdlib, so rebuilding the binary is the only way
/// to change it.
#[cfg(not(debug_assertions))]
fn hash_dev_stdlib(_hasher: &mut Sha256) {}

/// Length-prefixed, so no two sequences of fields hash alike.
pub(crate) fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// A file a compile looked up, known by its contents, or by its absence.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Input {
    pub(crate) path: PathBuf,
    /// How many leading bytes the compile depended on, as a Kiln input cut to
    /// its probe's extent; `None` is the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prefix: Option<u64>,
    /// `None` where the file was absent.
    sha256: Option<String>,
}

impl Input {
    pub(crate) fn of(path: PathBuf, bytes: Option<&[u8]>) -> Self {
        Self::hashed(path, None, bytes.map(|bytes| Sha256::digest(bytes).into()))
    }

    /// An input whose bytes the caller has already hashed.
    pub(crate) fn hashed(path: PathBuf, prefix: Option<u64>, sha256: Option<[u8; 32]>) -> Self {
        Self {
            path,
            prefix,
            sha256: sha256.map(|hash| hex32(&hash)),
        }
    }

    pub(crate) fn unchanged(&self) -> bool {
        Self::on_disk(self.path.clone(), self.prefix).as_ref() == Some(self)
    }

    /// What `path` holds now, its first `prefix` bytes where that is given;
    /// `None` where it cannot be read, which matches no recorded input. A path
    /// through a file, such as `wado.toml` under a path dependency naming a
    /// file, is as absent as a missing one.
    pub(crate) fn on_disk(path: PathBuf, prefix: Option<u64>) -> Option<Self> {
        match read_prefix(&path, prefix) {
            Ok(bytes) => Some(Self::hashed(
                path,
                prefix,
                Some(Sha256::digest(bytes).into()),
            )),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                Some(Self::hashed(path, prefix, None))
            }
            Err(_) => None,
        }
    }
}
