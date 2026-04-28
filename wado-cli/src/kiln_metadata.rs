//! Per-invocation Kiln cache state, stored at
//! `<manifest_root>/build/kiln/<invocation_id>/metadata.json`.
//!
//! See [WEP: Kiln](../../docs/wep-2026-04-12-kiln.md), section
//! "Caching and `metadata.json`", for the design.
//!
//! Always lives under `build/kiln/` regardless of where `output_dir`
//! points the generated `.wado` source — keeps cache state gitignored
//! even for committed-source workflows. Deleting the file (or the whole
//! `build/kiln/` tree) is not an error: the next compile rebuilds from
//! scratch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Schema version of the `metadata.json` file. Bumped only on
/// incompatible changes; older files are silently ignored (treated as a
/// cache miss) when the version differs.
pub const METADATA_VERSION: u32 = 1;

const METADATA_FILE: &str = "metadata.json";
const KILN_DIR: &str = "build/kiln";

/// Per-invocation cache state recorded between Kiln runs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    pub version: u32,
    pub invocation: String,
    pub generator: String,
    pub primary: FileHash,
    #[serde(default)]
    pub inputs: Vec<FileHash>,
    #[serde(default)]
    pub reads: Vec<FileHash>,
    pub options_hash: String,
    pub outputs: Vec<OutputEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileHash {
    pub path: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputEntry {
    pub path: String,
    pub hash: String,
    pub entry: bool,
}

/// Path to the metadata file for an invocation.
#[must_use]
pub fn metadata_path(manifest_root: &Path, invocation_id: &str) -> PathBuf {
    manifest_root
        .join(KILN_DIR)
        .join(invocation_id)
        .join(METADATA_FILE)
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
pub fn load(manifest_root: &Path, invocation_id: &str) -> Result<Option<Metadata>, MetadataError> {
    let path = metadata_path(manifest_root, invocation_id);
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

/// Write metadata.json, creating the parent directory if needed.
pub fn save(
    manifest_root: &Path,
    invocation_id: &str,
    metadata: &Metadata,
) -> Result<(), MetadataError> {
    let path = metadata_path(manifest_root, invocation_id);
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
            version: 1,
            invocation: "kiln-deadbeef".to_string(),
            generator: "local:src/generator.wado".to_string(),
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

    #[test]
    fn roundtrip_save_load() {
        let tmp = tempfile::tempdir().unwrap();
        let m = sample();
        save(tmp.path(), &m.invocation, &m).unwrap();
        let loaded = load(tmp.path(), &m.invocation).unwrap().unwrap();
        assert_eq!(loaded, m);
    }

    #[test]
    fn missing_file_is_cache_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load(tmp.path(), "kiln-nope").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn version_mismatch_is_cache_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let m = sample();
        let path = metadata_path(tmp.path(), &m.invocation);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Write with an unsupported version.
        let mut bumped = m.clone();
        bumped.version = u32::MAX;
        let json = serde_json::to_string(&bumped).unwrap();
        std::fs::write(&path, json).unwrap();
        assert!(load(tmp.path(), &m.invocation).unwrap().is_none());
    }

    #[test]
    fn parse_error_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = metadata_path(tmp.path(), "kiln-broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let err = load(tmp.path(), "kiln-broken").unwrap_err();
        assert!(matches!(err, MetadataError::Parse { .. }), "{err}");
    }

    #[test]
    fn metadata_path_layout() {
        let p = metadata_path(Path::new("/proj"), "kiln-abc");
        assert_eq!(p, PathBuf::from("/proj/build/kiln/kiln-abc/metadata.json"));
    }
}
