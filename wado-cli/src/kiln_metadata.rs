//! Per-invocation Kiln cache state, stored next to the generated outputs
//! at `<manifest_root>/<output_dir>/<primary_basename>.kiln.json`.
//!
//! See [WEP: Kiln](../../docs/wep-2026-04-12-kiln.md), section
//! "Caching and the `<primary>.kiln.json` cache file", for the design.
//!
//! The location is derived deterministically from `Invocation::output_dir`
//! and the basename of `Invocation::from` (the primary input file). For
//! invocations that opt into a committed `output_dir` (e.g.
//! `tests/generated/sqlite`), this means the cache state can be committed
//! alongside the generated `.wado` so a fresh checkout hits the cache and
//! skips the generator. For default invocations (`output_dir` under
//! `build/kiln/<synthetic_id>`), the file lives in that gitignored tree
//! and behaves like before. Deleting the file is not an error: the next
//! compile rebuilds from scratch.
//!
//! The schema types (`Metadata`, `FileHash`, `OutputEntry`,
//! `METADATA_VERSION`) live in `wado_compiler::kiln::metadata` so they
//! can be shared with `wado-lsp`'s consume-only redirect builder; this
//! module wraps them with `std::fs` reads and writes.

use std::path::{Path, PathBuf};

pub use wado_compiler::kiln::metadata::{
    FileHash, METADATA_VERSION, Metadata, OutputEntry, metadata_filename,
};

/// Path to the metadata file for an invocation.
///
/// `output_dir` is the relative `Invocation::output_dir` (e.g.
/// `tests/generated/sqlite` or `build/kiln/<synthetic_id>`). `primary`
/// is `Invocation::from` — the metadata's basename uses
/// [`metadata_filename`] so two invocations sharing one
/// `output_dir` collide only if they share both primary basename and
/// options, which would already make them the same invocation under
/// [`crate::kiln_driver::invocation_id`].
#[must_use]
pub fn metadata_path(manifest_root: &Path, output_dir: &str, primary: &str) -> PathBuf {
    manifest_root
        .join(output_dir)
        .join(metadata_filename(primary))
}

/// Read the metadata file for an invocation, if present.
///
/// `Ok(None)` means cache miss — the file does not exist, or its
/// declared `version` does not match [`METADATA_VERSION`]. In both cases
/// the caller should re-run the generator and overwrite the file. A
/// version mismatch is treated as cache-miss (not a hard error) so that
/// schema bumps do not require any manual intervention from the user.
///
/// Returns `Err` only on real I/O failure or syntactically broken JSON.
pub fn load(
    manifest_root: &Path,
    output_dir: &str,
    primary: &str,
) -> Result<Option<Metadata>, MetadataError> {
    let path = metadata_path(manifest_root, output_dir, primary);
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(MetadataError::Io { path, source }),
    };
    let metadata: Metadata = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(source) => {
            return Err(MetadataError::Parse {
                path,
                source: source.to_string(),
            });
        }
    };
    if metadata.version != METADATA_VERSION {
        return Ok(None);
    }
    Ok(Some(metadata))
}

/// Write the metadata file, creating the parent directory if needed.
pub fn save(
    manifest_root: &Path,
    output_dir: &str,
    primary: &str,
    metadata: &Metadata,
) -> Result<(), MetadataError> {
    let path = metadata_path(manifest_root, output_dir, primary);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| MetadataError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let json =
        serde_json::to_string_pretty(metadata).map_err(|source| MetadataError::Serialize {
            source: source.to_string(),
        })?;
    std::fs::write(&path, json).map_err(|source| MetadataError::Io { path, source })
}

#[derive(Debug)]
pub enum MetadataError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: String,
    },
    Serialize {
        source: String,
    },
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetadataError::Io { path, source } => write!(
                f,
                "kiln metadata: I/O error at {}: {source}",
                path.display()
            ),
            MetadataError::Parse { path, source } => {
                write!(
                    f,
                    "kiln metadata: parse error at {}: {source}",
                    path.display()
                )
            }
            MetadataError::Serialize { source } => {
                write!(f, "kiln metadata: serialize error: {source}")
            }
        }
    }
}

impl std::error::Error for MetadataError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Metadata {
        Metadata {
            version: METADATA_VERSION,
            invocation: "kiln-deadbeef".to_string(),
            generator: "local:src/generator.wado".to_string(),
            generator_source_hash: "sha256:gen".to_string(),
            primary: FileHash {
                path: "schemas/x.proto".to_string(),
                hash: "sha256:aa".to_string(),
            },
            inputs: vec![FileHash {
                path: "schemas/y.proto".to_string(),
                hash: "sha256:bb".to_string(),
            }],
            reads: vec![],
            options_hash: "sha256:cc".to_string(),
            outputs: vec![OutputEntry {
                path: "build/kiln/kiln-deadbeef/x.wado".to_string(),
                hash: "sha256:dd".to_string(),
                entry: true,
            }],
        }
    }

    const OUTPUT_DIR: &str = "tests/generated/x";
    const PRIMARY: &str = "schemas/x.proto";

    #[test]
    fn roundtrip_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let m = sample();
        save(tmp.path(), OUTPUT_DIR, PRIMARY, &m).unwrap();
        let loaded = load(tmp.path(), OUTPUT_DIR, PRIMARY).unwrap().unwrap();
        assert_eq!(loaded, m);
    }

    #[test]
    fn missing_file_is_cache_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load(tmp.path(), OUTPUT_DIR, PRIMARY).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn version_mismatch_is_cache_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bumped = sample();
        let path = metadata_path(tmp.path(), OUTPUT_DIR, PRIMARY);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Write with an unsupported version.
        bumped.version = u32::MAX;
        let json = serde_json::to_string(&bumped).unwrap();
        std::fs::write(&path, json).unwrap();
        assert!(load(tmp.path(), OUTPUT_DIR, PRIMARY).unwrap().is_none());
    }

    #[test]
    fn parse_error_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = metadata_path(tmp.path(), OUTPUT_DIR, PRIMARY);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let err = load(tmp.path(), OUTPUT_DIR, PRIMARY).unwrap_err();
        assert!(matches!(err, MetadataError::Parse { .. }), "{err}");
    }

    #[test]
    fn metadata_path_uses_primary_basename_in_output_dir() {
        let p = metadata_path(
            Path::new("/proj"),
            "tests/generated/sqlite",
            "tests/grammars/SQLite.g4",
        );
        assert_eq!(
            p,
            PathBuf::from("/proj/tests/generated/sqlite/SQLite.g4.kiln.json")
        );
    }

    #[test]
    fn metadata_path_handles_bare_primary_filename() {
        let p = metadata_path(Path::new("/proj"), "build/kiln/synth-id", "schema.proto");
        assert_eq!(
            p,
            PathBuf::from("/proj/build/kiln/synth-id/schema.proto.kiln.json")
        );
    }
}
