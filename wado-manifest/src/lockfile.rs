use std::fmt::{self, Write as _};
use std::str::FromStr;

use serde::Deserialize;

/// A parsed `wado.lock` file.
#[derive(Debug, Clone)]
pub struct LockFile {
    /// Lock file format version.
    pub version: u32,
    /// Hash of `[dependencies]` + `[dev-dependencies]` for staleness detection.
    pub deps_hash: String,
    /// Resolved packages.
    pub packages: Vec<LockedPackage>,
    /// Resolved build dependencies (Kiln generator modules).
    pub build_dependencies: Vec<LockedPackage>,
    /// Kiln generator-cache entries, one per invocation.
    pub generator_cache: Vec<GeneratorCacheEntry>,
}

/// A single `[[generator-cache]]` entry recording the last run of a Kiln
/// generator invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratorCacheEntry {
    /// Synthesized invocation id derived from `(decl_file, from)`.
    pub invocation: String,
    /// Generator identity as resolved: `"ns:name@version"` plus any module path.
    pub generator: String,
    /// Primary schema file hash.
    pub primary: FileHash,
    /// Declared supplementary input hashes.
    pub inputs: Vec<FileHash>,
    /// Hex SHA-256 of the canonical options encoding.
    pub options_hash: String,
    /// Files the generator read via `host::read-file` during the last run,
    /// sorted lexicographically by path and deduplicated. Fed into the next
    /// run's cache key so transitive schema reads invalidate the cache when
    /// their contents change. Empty on first run.
    pub reads: Vec<FileHash>,
    /// Files produced by the last run.
    pub outputs: Vec<OutputHash>,
}

/// Hash of an input file used by a generator invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHash {
    /// Path relative to the manifest directory, forward-slash normalized.
    pub path: String,
    /// Hex SHA-256 of the file contents.
    pub hash: String,
}

/// Hash of an output file produced by a generator invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputHash {
    /// Path relative to the manifest directory, forward-slash normalized.
    pub path: String,
    /// Hex SHA-256 of the file contents.
    pub hash: String,
    /// Whether this file is the invocation's entry module.
    pub entry: bool,
}

/// A single resolved package in the lock file.
#[derive(Debug, Clone)]
pub struct LockedPackage {
    /// Resolved package id (e.g., `"registry+https://wa.dev/docs:regex"`).
    pub id: String,
    /// Exact resolved version.
    pub version: String,
    /// Exact commit SHA (git dependencies only).
    pub resolved_ref: Option<String>,
    /// Content hash (registry dependencies only).
    pub integrity: Option<String>,
    /// Whether this is a dev-only dependency.
    pub dev: bool,
    /// Entry point for `wasi:cli/command`.
    pub command: Option<String>,
    /// Entry point for `wasi:http/service`.
    pub service: Option<String>,
    /// Library entry point.
    pub lib: Option<String>,
    /// Dependencies as `"id@version"` references.
    pub deps: Vec<String>,
}

impl FromStr for LockFile {
    type Err = LockFileError;

    fn from_str(toml_str: &str) -> Result<Self, Self::Err> {
        let raw: RawLockFile = toml::from_str::<RawLockFile>(toml_str)
            .map_err(|e| LockFileError::Toml(e.to_string()))?;

        let version = raw.version.ok_or_else(|| LockFileError::MissingField {
            package_id: "(header)".to_string(),
            field: "version".to_string(),
        })?;

        if version != 1 {
            return Err(LockFileError::InvalidVersion {
                expected: 1,
                found: version,
            });
        }

        let deps_hash = raw.deps_hash.ok_or_else(|| LockFileError::MissingField {
            package_id: "(header)".to_string(),
            field: "deps-hash".to_string(),
        })?;

        let packages = raw
            .package
            .unwrap_or_default()
            .into_iter()
            .map(convert_locked_package)
            .collect::<Result<Vec<_>, _>>()?;

        let build_dependencies = raw
            .build_dependency
            .unwrap_or_default()
            .into_iter()
            .map(convert_locked_package)
            .collect::<Result<Vec<_>, _>>()?;

        let generator_cache = raw
            .generator_cache
            .unwrap_or_default()
            .into_iter()
            .map(convert_generator_cache_entry)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(LockFile {
            version,
            deps_hash,
            packages,
            build_dependencies,
            generator_cache,
        })
    }
}

impl LockFile {
    /// Serialize to deterministic TOML string.
    ///
    /// Packages are sorted by `id` then `version` for determinism. Build
    /// dependencies appear as `[[build-dependency]]` sections sorted the same
    /// way. Generator-cache entries appear as `[[generator-cache]]` sections
    /// in declaration order, as supplied — the caller is responsible for
    /// producing them in the order users see in `wado.toml`.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        out.push_str("# This file is auto-generated by wado. Do not edit manually.\n");
        let _ = writeln!(out, "version = {}", self.version);
        let _ = writeln!(out, "deps-hash = {:?}", self.deps_hash);

        let mut sorted: Vec<&LockedPackage> = self.packages.iter().collect();
        sorted.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.version.cmp(&b.version)));
        for pkg in sorted {
            write_locked_package(&mut out, pkg, "[[package]]");
        }

        let mut sorted_build: Vec<&LockedPackage> = self.build_dependencies.iter().collect();
        sorted_build.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.version.cmp(&b.version)));
        for pkg in sorted_build {
            write_locked_package(&mut out, pkg, "[[build-dependency]]");
        }

        for entry in &self.generator_cache {
            write_generator_cache_entry(&mut out, entry);
        }

        out
    }
}

fn write_locked_package(out: &mut String, pkg: &LockedPackage, header: &str) {
    out.push('\n');
    out.push_str(header);
    out.push('\n');
    let _ = writeln!(out, "id = {:?}", pkg.id);
    let _ = writeln!(out, "version = {:?}", pkg.version);
    if let Some(resolved_ref) = &pkg.resolved_ref {
        let _ = writeln!(out, "resolved-ref = {resolved_ref:?}");
    }
    if let Some(integrity) = &pkg.integrity {
        let _ = writeln!(out, "integrity = {integrity:?}");
    }
    if pkg.dev {
        out.push_str("dev = true\n");
    }
    if let Some(command) = &pkg.command {
        let _ = writeln!(out, "command = {command:?}");
    }
    if let Some(service) = &pkg.service {
        let _ = writeln!(out, "service = {service:?}");
    }
    if let Some(lib) = &pkg.lib {
        let _ = writeln!(out, "lib = {lib:?}");
    }
    if pkg.deps.is_empty() {
        out.push_str("deps = []\n");
    } else {
        out.push_str("deps = [");
        for (i, dep) in pkg.deps.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{dep:?}");
        }
        out.push_str("]\n");
    }
}

fn write_generator_cache_entry(out: &mut String, entry: &GeneratorCacheEntry) {
    out.push('\n');
    out.push_str("[[generator-cache]]\n");
    let _ = writeln!(out, "invocation = {:?}", entry.invocation);
    let _ = writeln!(out, "generator = {:?}", entry.generator);
    let _ = writeln!(
        out,
        "primary = {{ path = {:?}, hash = {:?} }}",
        entry.primary.path, entry.primary.hash
    );
    let _ = writeln!(out, "options-hash = {:?}", entry.options_hash);

    if entry.inputs.is_empty() {
        out.push_str("inputs = []\n");
    } else {
        out.push_str("inputs = [\n");
        for file in &entry.inputs {
            let _ = writeln!(
                out,
                "    {{ path = {:?}, hash = {:?} }},",
                file.path, file.hash
            );
        }
        out.push_str("]\n");
    }

    if entry.reads.is_empty() {
        out.push_str("reads = []\n");
    } else {
        out.push_str("reads = [\n");
        for file in &entry.reads {
            let _ = writeln!(
                out,
                "    {{ path = {:?}, hash = {:?} }},",
                file.path, file.hash
            );
        }
        out.push_str("]\n");
    }

    if entry.outputs.is_empty() {
        out.push_str("outputs = []\n");
    } else {
        out.push_str("outputs = [\n");
        for file in &entry.outputs {
            let _ = writeln!(
                out,
                "    {{ path = {:?}, hash = {:?}, entry = {} }},",
                file.path, file.hash, file.entry
            );
        }
        out.push_str("]\n");
    }
}

/// Errors from lock file parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockFileError {
    /// TOML syntax error.
    Toml(String),
    /// Unsupported lock file version.
    InvalidVersion { expected: u32, found: u32 },
    /// Missing required field.
    MissingField { package_id: String, field: String },
}

impl fmt::Display for LockFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockFileError::Toml(msg) => write!(f, "lock file TOML error: {msg}"),
            LockFileError::InvalidVersion { expected, found } => {
                write!(
                    f,
                    "unsupported lock file version {found} (expected {expected})"
                )
            }
            LockFileError::MissingField { package_id, field } => {
                write!(f, "missing field `{field}` in lock file ({package_id})")
            }
        }
    }
}

impl std::error::Error for LockFileError {}

// --- Raw serde types ---

#[derive(Deserialize)]
struct RawLockFile {
    version: Option<u32>,
    #[serde(rename = "deps-hash")]
    deps_hash: Option<String>,
    package: Option<Vec<RawLockedPackage>>,
    #[serde(rename = "build-dependency")]
    build_dependency: Option<Vec<RawLockedPackage>>,
    #[serde(rename = "generator-cache")]
    generator_cache: Option<Vec<RawGeneratorCacheEntry>>,
}

#[derive(Deserialize)]
struct RawGeneratorCacheEntry {
    invocation: Option<String>,
    generator: Option<String>,
    primary: Option<RawFileHash>,
    inputs: Option<Vec<RawFileHash>>,
    #[serde(rename = "options-hash")]
    options_hash: Option<String>,
    reads: Option<Vec<RawFileHash>>,
    outputs: Option<Vec<RawOutputHash>>,
}

#[derive(Deserialize)]
struct RawFileHash {
    path: Option<String>,
    hash: Option<String>,
}

#[derive(Deserialize)]
struct RawOutputHash {
    path: Option<String>,
    hash: Option<String>,
    entry: Option<bool>,
}

#[derive(Deserialize)]
struct RawLockedPackage {
    id: Option<String>,
    version: Option<String>,
    #[serde(rename = "resolved-ref")]
    resolved_ref: Option<String>,
    integrity: Option<String>,
    dev: Option<bool>,
    command: Option<String>,
    service: Option<String>,
    lib: Option<String>,
    deps: Option<Vec<String>>,
}

fn convert_generator_cache_entry(
    raw: RawGeneratorCacheEntry,
) -> Result<GeneratorCacheEntry, LockFileError> {
    let invocation = raw.invocation.ok_or_else(|| LockFileError::MissingField {
        package_id: "(generator-cache)".to_string(),
        field: "invocation".to_string(),
    })?;
    let generator = raw.generator.ok_or_else(|| LockFileError::MissingField {
        package_id: format!("generator-cache/{invocation}"),
        field: "generator".to_string(),
    })?;
    let primary_raw = raw.primary.ok_or_else(|| LockFileError::MissingField {
        package_id: format!("generator-cache/{invocation}"),
        field: "primary".to_string(),
    })?;
    let primary = convert_file_hash(&invocation, "primary", primary_raw)?;
    let options_hash = raw
        .options_hash
        .ok_or_else(|| LockFileError::MissingField {
            package_id: format!("generator-cache/{invocation}"),
            field: "options-hash".to_string(),
        })?;
    let inputs = raw
        .inputs
        .unwrap_or_default()
        .into_iter()
        .map(|f| convert_file_hash(&invocation, "inputs", f))
        .collect::<Result<Vec<_>, _>>()?;
    let reads = raw
        .reads
        .unwrap_or_default()
        .into_iter()
        .map(|f| convert_file_hash(&invocation, "reads", f))
        .collect::<Result<Vec<_>, _>>()?;
    let outputs = raw
        .outputs
        .unwrap_or_default()
        .into_iter()
        .map(|f| convert_output_hash(&invocation, f))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GeneratorCacheEntry {
        invocation,
        generator,
        primary,
        inputs,
        options_hash,
        reads,
        outputs,
    })
}

fn convert_file_hash(
    invocation: &str,
    field: &str,
    raw: RawFileHash,
) -> Result<FileHash, LockFileError> {
    let path = raw.path.ok_or_else(|| LockFileError::MissingField {
        package_id: format!("generator-cache/{invocation}"),
        field: format!("{field}.path"),
    })?;
    let hash = raw.hash.ok_or_else(|| LockFileError::MissingField {
        package_id: format!("generator-cache/{invocation}"),
        field: format!("{field}.hash"),
    })?;
    Ok(FileHash { path, hash })
}

fn convert_output_hash(invocation: &str, raw: RawOutputHash) -> Result<OutputHash, LockFileError> {
    let path = raw.path.ok_or_else(|| LockFileError::MissingField {
        package_id: format!("generator-cache/{invocation}"),
        field: "outputs.path".to_string(),
    })?;
    let hash = raw.hash.ok_or_else(|| LockFileError::MissingField {
        package_id: format!("generator-cache/{invocation}"),
        field: "outputs.hash".to_string(),
    })?;
    Ok(OutputHash {
        path,
        hash,
        entry: raw.entry.unwrap_or(false),
    })
}

fn convert_locked_package(raw: RawLockedPackage) -> Result<LockedPackage, LockFileError> {
    let id = raw.id.ok_or_else(|| LockFileError::MissingField {
        package_id: "(unknown)".to_string(),
        field: "id".to_string(),
    })?;
    let version = raw.version.ok_or_else(|| LockFileError::MissingField {
        package_id: id.clone(),
        field: "version".to_string(),
    })?;
    Ok(LockedPackage {
        id,
        version,
        resolved_ref: raw.resolved_ref,
        integrity: raw.integrity,
        dev: raw.dev.unwrap_or(false),
        command: raw.command,
        service: raw.service,
        lib: raw.lib,
        deps: raw.deps.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_LOCK: &str = r#"
# This file is auto-generated by wado. Do not edit manually.
version = 1
deps-hash = "sha256:9f8e7d6c5b4a"

[[package]]
id = "registry+https://wa.dev/docs:regex"
version = "0.1.2"
integrity = "sha256:a1b2c3d4e5f6"
lib = "src/lib.wado"
deps = ["registry+https://wa.dev/docs:regex-utils@0.3.0"]

[[package]]
id = "registry+https://wa.dev/docs:regex-utils"
version = "0.3.0"
integrity = "sha256:f6e5d4c3b2a1"
lib = "src/lib.wado"
deps = []

[[package]]
id = "git+https://github.com/user/router.git/user:router"
version = "1.0.2"
resolved-ref = "abc1234def5678901234567890abcdef12345678"
lib = "src/lib.wado"
deps = ["registry+https://wa.dev/tools:utils@0.5.1"]
"#;

    #[test]
    fn parse_lock_file() {
        let lock: LockFile = EXAMPLE_LOCK.parse().unwrap();
        assert_eq!(lock.version, 1);
        assert_eq!(lock.deps_hash, "sha256:9f8e7d6c5b4a");
        assert_eq!(lock.packages.len(), 3);

        let regex = &lock.packages[0];
        assert_eq!(regex.id, "registry+https://wa.dev/docs:regex");
        assert_eq!(regex.version, "0.1.2");
        assert_eq!(regex.integrity.as_deref(), Some("sha256:a1b2c3d4e5f6"));
        assert!(!regex.dev);
        assert_eq!(regex.deps.len(), 1);

        let router = &lock.packages[2];
        assert!(router.resolved_ref.is_some());
        assert!(router.integrity.is_none());
    }

    #[test]
    fn roundtrip() {
        let lock: LockFile = EXAMPLE_LOCK.parse().unwrap();
        let serialized = lock.to_toml();
        let reparsed: LockFile = serialized.parse().unwrap();
        assert_eq!(reparsed.version, lock.version);
        assert_eq!(reparsed.deps_hash, lock.deps_hash);
        assert_eq!(reparsed.packages.len(), lock.packages.len());
    }

    #[test]
    fn invalid_version() {
        let toml = r#"
version = 99
deps-hash = "sha256:abc"
"#;
        let err = toml.parse::<LockFile>().unwrap_err();
        assert!(matches!(
            err,
            LockFileError::InvalidVersion {
                expected: 1,
                found: 99
            }
        ));
    }

    #[test]
    fn dev_dep_flag() {
        let toml = r#"
version = 1
deps-hash = "sha256:abc"

[[package]]
id = "git+https://gitlab.com/user/bench.git/bench-tool"
version = "0.1.0"
resolved-ref = "def5678901234567890abcdef12345678abc1234d"
dev = true
command = "src/main.wado"
deps = []
"#;
        let lock: LockFile = toml.parse().unwrap();
        assert!(lock.packages[0].dev);
        assert_eq!(lock.packages[0].command.as_deref(), Some("src/main.wado"));
    }

    #[test]
    fn deterministic_output() {
        let lock = LockFile {
            version: 1,
            deps_hash: "sha256:test".to_string(),
            packages: vec![
                LockedPackage {
                    id: "registry+https://wa.dev/z:last".to_string(),
                    version: "1.0.0".to_string(),
                    resolved_ref: None,
                    integrity: Some("sha256:zzz".to_string()),
                    dev: false,
                    command: None,
                    service: None,
                    lib: Some("src/lib.wado".to_string()),
                    deps: vec![],
                },
                LockedPackage {
                    id: "registry+https://wa.dev/a:first".to_string(),
                    version: "0.1.0".to_string(),
                    resolved_ref: None,
                    integrity: Some("sha256:aaa".to_string()),
                    dev: false,
                    command: None,
                    service: None,
                    lib: Some("src/lib.wado".to_string()),
                    deps: vec![],
                },
            ],
            build_dependencies: vec![],
            generator_cache: vec![],
        };
        let output = lock.to_toml();
        let pos_a = output.find("a:first").unwrap();
        let pos_z = output.find("z:last").unwrap();
        assert!(pos_a < pos_z, "packages should be sorted by id");
    }

    const EXAMPLE_KILN_LOCK: &str = r#"
version = 1
deps-hash = "sha256:9f8e7d6c5b4a"

[[package]]
id = "registry+https://wa.dev/docs:regex"
version = "0.1.2"
integrity = "sha256:aaa"
lib = "src/lib.wado"
deps = []

[[build-dependency]]
id = "registry+https://wa.dev/tools:protoc-gen"
version = "2.3.0"
integrity = "sha256:bbb"
lib = "src/lib.wado"
deps = []

[[build-dependency]]
id = "registry+https://wa.dev/tools:antlr-gen"
version = "1.0.0"
integrity = "sha256:ccc"
lib = "src/lib.wado"
deps = []

[[generator-cache]]
invocation = "proto-greeter"
generator = "tools:protoc-gen@2.3.0"
primary = { path = "schemas/greeter.proto", hash = "sha256:schema1" }
options-hash = "sha256:opt1"
inputs = [
    { path = "schemas/common.proto", hash = "sha256:schema2" },
]
reads = [
    { path = "schemas/imported.proto", hash = "sha256:read1" },
    { path = "schemas/shared.proto", hash = "sha256:read2" },
]
outputs = [
    { path = "build/kiln/proto-greeter/greeter.wado", hash = "sha256:out1", entry = true },
    { path = "build/kiln/proto-greeter/types.wado", hash = "sha256:out2", entry = false },
]

[[generator-cache]]
invocation = "antlr-lang"
generator = "tools:antlr-gen@1.0.0"
primary = { path = "grammars/lang.g4", hash = "sha256:schema3" }
options-hash = "sha256:opt2"
inputs = []
outputs = [
    { path = "build/kiln/antlr-lang/parser.wado", hash = "sha256:out3", entry = true },
]
"#;

    #[test]
    fn parse_kiln_sections() {
        let lock: LockFile = EXAMPLE_KILN_LOCK.parse().unwrap();
        assert_eq!(lock.build_dependencies.len(), 2);
        assert_eq!(lock.generator_cache.len(), 2);

        let protoc = lock
            .build_dependencies
            .iter()
            .find(|p| p.id.contains("protoc-gen"))
            .unwrap();
        assert_eq!(protoc.version, "2.3.0");

        let proto_entry = lock
            .generator_cache
            .iter()
            .find(|e| e.invocation == "proto-greeter")
            .unwrap();
        assert_eq!(proto_entry.generator, "tools:protoc-gen@2.3.0");
        assert_eq!(proto_entry.primary.path, "schemas/greeter.proto");
        assert_eq!(proto_entry.primary.hash, "sha256:schema1");
        assert_eq!(proto_entry.inputs.len(), 1);
        assert_eq!(proto_entry.reads.len(), 2);
        assert_eq!(proto_entry.reads[0].path, "schemas/imported.proto");
        assert_eq!(proto_entry.reads[1].path, "schemas/shared.proto");
        assert_eq!(proto_entry.outputs.len(), 2);
        assert!(proto_entry.outputs[0].entry);
        assert!(!proto_entry.outputs[1].entry);

        let antlr_entry = lock
            .generator_cache
            .iter()
            .find(|e| e.invocation == "antlr-lang")
            .unwrap();
        assert!(antlr_entry.inputs.is_empty());
        assert_eq!(antlr_entry.outputs.len(), 1);
    }

    #[test]
    fn roundtrip_kiln_sections() {
        let lock: LockFile = EXAMPLE_KILN_LOCK.parse().unwrap();
        let serialized = lock.to_toml();
        let reparsed: LockFile = serialized.parse().unwrap();
        assert_eq!(reparsed.build_dependencies.len(), 2);
        assert_eq!(reparsed.generator_cache.len(), 2);
        assert_eq!(
            reparsed
                .generator_cache
                .iter()
                .map(|e| e.invocation.clone())
                .collect::<Vec<_>>(),
            lock.generator_cache
                .iter()
                .map(|e| e.invocation.clone())
                .collect::<Vec<_>>(),
            "generator-cache order must survive round-trip (declaration order)"
        );
        let proto_reparsed = reparsed
            .generator_cache
            .iter()
            .find(|e| e.invocation == "proto-greeter")
            .unwrap();
        assert_eq!(proto_reparsed.reads.len(), 2);
        assert_eq!(proto_reparsed.reads[0].path, "schemas/imported.proto");
        assert_eq!(proto_reparsed.reads[0].hash, "sha256:read1");
    }

    #[test]
    fn deterministic_kiln_ordering() {
        let lock = LockFile {
            version: 1,
            deps_hash: "sha256:test".to_string(),
            packages: vec![],
            build_dependencies: vec![
                LockedPackage {
                    id: "registry+https://wa.dev/tools:z-gen".to_string(),
                    version: "1.0.0".to_string(),
                    resolved_ref: None,
                    integrity: Some("sha256:zzz".to_string()),
                    dev: false,
                    command: None,
                    service: None,
                    lib: Some("src/lib.wado".to_string()),
                    deps: vec![],
                },
                LockedPackage {
                    id: "registry+https://wa.dev/tools:a-gen".to_string(),
                    version: "0.1.0".to_string(),
                    resolved_ref: None,
                    integrity: Some("sha256:aaa".to_string()),
                    dev: false,
                    command: None,
                    service: None,
                    lib: Some("src/lib.wado".to_string()),
                    deps: vec![],
                },
            ],
            generator_cache: vec![
                GeneratorCacheEntry {
                    invocation: "z-invoke".to_string(),
                    generator: "tools:z-gen@1.0.0".to_string(),
                    primary: FileHash {
                        path: "z.schema".to_string(),
                        hash: "sha256:z".to_string(),
                    },
                    inputs: vec![],
                    options_hash: "sha256:zo".to_string(),
                    reads: vec![],
                    outputs: vec![],
                },
                GeneratorCacheEntry {
                    invocation: "a-invoke".to_string(),
                    generator: "tools:a-gen@0.1.0".to_string(),
                    primary: FileHash {
                        path: "a.schema".to_string(),
                        hash: "sha256:a".to_string(),
                    },
                    inputs: vec![],
                    options_hash: "sha256:ao".to_string(),
                    reads: vec![],
                    outputs: vec![],
                },
            ],
        };
        let output = lock.to_toml();
        let a_bd = output.find("tools:a-gen").unwrap();
        let z_bd = output.find("tools:z-gen").unwrap();
        assert!(a_bd < z_bd, "build-dependencies should be sorted by id");
        let z_cache = output.find("z-invoke").unwrap();
        let a_cache = output.find("a-invoke").unwrap();
        assert!(
            z_cache < a_cache,
            "generator-cache should follow declaration order (as supplied), not alphabetical"
        );
    }
}
