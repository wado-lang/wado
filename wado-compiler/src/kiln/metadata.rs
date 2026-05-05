//! Per-invocation Kiln cache file (`<output_dir>/<primary>.kiln.json`)
//! schema, shared between `wado-cli` (which reads and writes the file via
//! `std::fs`) and `wado-lsp` (which reads it for consume-only redirect
//! resolution).
//!
//! This module is intentionally I/O-free so the compiler crate keeps its
//! `wasm32-unknown-unknown` clean build. The native CLI host wraps it with
//! `std::fs` reads and writes; the LSP host (which builds for both native
//! and `wasm32-wasip2`) does its own narrow read.
//!
//! See [WEP: Kiln](../../../docs/wep-2026-04-12-kiln.md), section
//! "Caching and the `<primary>.kiln.json` cache file".

use serde::{Deserialize, Serialize};

/// Schema version of the `<primary>.kiln.json` file. Bumped only on
/// incompatible changes; older files are silently ignored (treated as a
/// cache miss) when the version differs.
///
/// v2 (current): added `generator_source_hash` so a change to the
/// generator's `.wado` source closure invalidates the cache.
pub const METADATA_VERSION: u32 = 2;

/// Suffix appended to the primary input's basename to form the metadata
/// filename. Lives in `<manifest_root>/<output_dir>/`.
pub const METADATA_SUFFIX: &str = ".kiln.json";

/// Per-invocation cache state recorded between Kiln runs.
///
/// `outputs[].path` is project-root-relative so committed-source workflows
/// can record paths like `src/generated/...`. `outputs[].hash` is recorded
/// only here — `wado.lock` does not carry it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    pub version: u32,
    pub invocation: String,
    pub generator: String,
    /// Hex-encoded SHA-256 of the generator's source closure (entry
    /// `.wado` plus every transitively imported `.wado`). When the
    /// stored value differs from the provider's current hash the
    /// cache must miss. An empty string is recorded for providers that
    /// cannot compute one (currently the spec-form path), which means
    /// cache validation falls back to the file-hash checks alone.
    #[serde(default)]
    pub generator_source_hash: String,
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

/// Compute the metadata filename (just the basename, without any
/// directory) for a given primary input path. Callers join this with
/// `<manifest_root>/<output_dir>` to obtain the full path.
#[must_use]
pub fn metadata_filename(primary: &str) -> String {
    let basename = std::path::Path::new(primary)
        .file_name()
        .map_or_else(|| primary.to_string(), |s| s.to_string_lossy().into_owned());
    format!("{basename}{METADATA_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_filename_strips_directory() {
        assert_eq!(metadata_filename("schemas/x.proto"), "x.proto.kiln.json");
        assert_eq!(metadata_filename("x.proto"), "x.proto.kiln.json");
        assert_eq!(
            metadata_filename("tests/grammars/Calc.g4"),
            "Calc.g4.kiln.json"
        );
    }

    #[test]
    fn metadata_roundtrips_through_serde_json() {
        let m = Metadata {
            version: METADATA_VERSION,
            invocation: "kiln-deadbeef".to_string(),
            generator: "local:src/generator.wado".to_string(),
            generator_source_hash: "sha256:gen".to_string(),
            primary: FileHash {
                path: "schemas/x.proto".to_string(),
                hash: "sha256:aa".to_string(),
            },
            inputs: vec![],
            reads: vec![],
            options_hash: "sha256:cc".to_string(),
            outputs: vec![OutputEntry {
                path: "build/kiln/kiln-deadbeef/x.wado".to_string(),
                hash: "sha256:dd".to_string(),
                entry: true,
            }],
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: Metadata = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }
}
