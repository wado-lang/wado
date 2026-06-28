use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use indexmap::IndexMap;
use serde::Deserialize;

use crate::validate;
use crate::version::VersionSpecifier;

/// A parsed `wado.toml` manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub package: Option<Package>,
    /// The `[world]` table: CM world FQ name (e.g. `"wasi:cli/command"`,
    /// `"core:kiln/generator"`) → entry-point path, one entry per hosted
    /// world the package targets. The library world is declared separately by
    /// `[package].lib` (its world name is the package name).
    pub world: IndexMap<String, String>,
    pub registries: IndexMap<String, String>,
    pub dependencies: IndexMap<String, Dependency>,
    pub dev_dependencies: IndexMap<String, Dependency>,
    pub build_dependencies: IndexMap<String, Dependency>,
    pub workspace: Option<Workspace>,
    pub test: TestSettings,
    pub format: FormatSettings,
    /// Unknown top-level keys/sections (typos, unsupported). Reported as
    /// warnings, never errors.
    pub unknown_sections: Vec<String>,
    /// Unknown `[workspace.package]` keys inherited from the workspace root
    /// during member resolution. Surfaced as `workspace.package` warnings on the
    /// member (whose own `workspace` is `None`). Empty otherwise.
    pub inherited_unknown_fields: Vec<String>,
}

impl Manifest {
    /// Entry-point source path for the given CM world FQ name, if declared in
    /// the `[world]` table.
    #[must_use]
    pub fn world_entry(&self, world_fq: &str) -> Option<&str> {
        self.world.get(world_fq).map(String::as_str)
    }

    /// Deterministic `sha256:` hash of `[dependencies]` + `[dev-dependencies]`
    /// for lock staleness. Sources are normalized so equal manifests hash equally.
    #[must_use]
    pub fn deps_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for (label, deps) in [
            ("deps", &self.dependencies),
            ("dev", &self.dev_dependencies),
        ] {
            let mut keys: Vec<&String> = deps.keys().collect();
            keys.sort();
            for key in keys {
                hasher.update(label.as_bytes());
                hasher.update(b"\0");
                hasher.update(key.as_bytes());
                hasher.update(b"\0");
                hasher.update(source_fingerprint(&deps[key].source).as_bytes());
                hasher.update(b"\n");
            }
        }
        let digest = hasher.finalize();
        let mut out = String::with_capacity(7 + 64);
        out.push_str("sha256:");
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[must_use]
    pub fn warnings(&self) -> Vec<ManifestWarning> {
        let keys = self
            .dependencies
            .keys()
            .chain(self.dev_dependencies.keys())
            .chain(self.build_dependencies.keys());
        let mut warnings: Vec<ManifestWarning> = keys
            .filter(|k| !k.contains(':'))
            .map(|k| ManifestWarning::BareDependencyKey { key: k.clone() })
            .collect();
        for field in &self.unknown_sections {
            warnings.push(ManifestWarning::UnknownField {
                section: None,
                field: field.clone(),
            });
        }
        if let Some(pkg) = &self.package {
            for field in &pkg.unknown_fields {
                warnings.push(ManifestWarning::UnknownField {
                    section: Some("package".to_string()),
                    field: field.clone(),
                });
            }
        }
        let workspace_pkg_unknowns = self
            .workspace
            .as_ref()
            .and_then(|ws| ws.package.as_ref())
            .map(|p| p.unknown_fields.as_slice())
            .unwrap_or_default()
            .iter()
            .chain(&self.inherited_unknown_fields);
        for field in workspace_pkg_unknowns {
            warnings.push(ManifestWarning::UnknownField {
                section: Some("workspace.package".to_string()),
                field: field.clone(),
            });
        }
        warnings
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestWarning {
    /// Bare key (no `:`); deprecated in favor of `ns:pkg` / `lib:nick` (WEP).
    BareDependencyKey { key: String },
    /// Unknown key not recognized by the schema. `section` is the table it
    /// appeared in (`None` for a top-level key/section).
    UnknownField {
        section: Option<String>,
        field: String,
    },
}

impl std::fmt::Display for ManifestWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestWarning::BareDependencyKey { key } => write!(
                f,
                "dependency key {key:?} is a bare name (deprecated); use a coordinate \
                 like \"ns:{key}\" or a \"lib:{key}\" indirection",
            ),
            ManifestWarning::UnknownField {
                section: Some(section),
                field,
            } => write!(f, "unknown field {field:?} in [{section}] (ignored)"),
            ManifestWarning::UnknownField {
                section: None,
                field,
            } => write!(f, "unknown top-level key {field:?} (ignored)"),
        }
    }
}

/// The `[format]` section of `wado.toml`.
///
/// Controls which `*.wado` files `wado format` discovers when given a
/// directory. Mirrors `[test]`: `exclude` drops paths, `include` carves them
/// back in. Typical use is excluding hand-authored e2e fixtures whose layout
/// is part of the test and must not be rewritten by the formatter.
#[derive(Debug, Clone, Default)]
pub struct FormatSettings {
    /// Glob patterns (relative to the manifest root) for paths to exclude from
    /// format discovery.
    pub exclude: Vec<String>,
    /// Glob patterns (relative to the manifest root) for paths to keep in
    /// format discovery even when they would otherwise be excluded. Patterns
    /// here win over `exclude`.
    pub include: Vec<String>,
}

/// The `[test]` section of `wado.toml`.
///
/// Controls how `wado test` discovers and runs `*.wado` files in the package.
#[derive(Debug, Clone, Default)]
pub struct TestSettings {
    /// Glob patterns (relative to the package root) for paths to exclude from
    /// test discovery. See WEP 2026-05-02.
    pub exclude: Vec<String>,
    /// Glob patterns (relative to the package root) for paths to keep in test
    /// discovery even when they would otherwise be excluded. Patterns here win
    /// over `exclude` — typical use is `lib/**/*_test.wado` inside a stdlib
    /// package whose non-test sources have to be excluded (e.g. because they
    /// re-export the prelude and can't compile as their own entry).
    pub include: Vec<String>,
}

/// The `[package]` section of `wado.toml`.
#[derive(Debug, Clone)]
pub struct Package {
    pub namespace: Option<String>,
    pub name: String,
    pub version: String,
    /// Entry-point module exposed when the package is consumed as a
    /// dependency (`use { … } from "<dep-name>"`). Only `export` items are
    /// visible to consumers.
    pub lib: Option<String>,
    /// Short, human-readable summary (→ `org.opencontainers.image.description`).
    pub description: Option<String>,
    /// Project home page URL (→ `org.opencontainers.image.url`). Falls back to
    /// `repository` when unset; see [`Package::effective_homepage`].
    pub homepage: Option<String>,
    /// Source repository URL, bare (no subdirectory)
    /// (→ `org.opencontainers.image.source`).
    pub repository: Option<String>,
    /// Subdirectory holding the package within a monorepo. Wado-custom; not an
    /// OCI annotation.
    pub repository_directory: Option<String>,
    /// Documentation URL (→ `org.opencontainers.image.documentation`). Falls
    /// back to `repository`; see [`Package::effective_documentation`].
    pub documentation: Option<String>,
    /// SPDX License Expression (→ `org.opencontainers.image.licenses`).
    /// Mutually exclusive with `license_file`.
    pub license: Option<String>,
    /// Path to a non-standard license file. Mutually exclusive with `license`.
    pub license_file: Option<String>,
    /// Contact details of the people/organization responsible
    /// (→ `org.opencontainers.image.authors`).
    pub authors: Vec<String>,
    /// Minimum Wado compiler version required to build (a semver requirement,
    /// e.g. `">=0.5"`).
    pub wado_version: Option<String>,
    /// Whether the package may be published. `false` opts out even when a
    /// `namespace` is present. Defaults to `true`.
    pub publish: bool,
    /// Unknown `[package]` keys (typos, unsupported). Reported as warnings.
    pub unknown_fields: Vec<String>,
}

impl Package {
    /// Homepage, falling back to the repository URL when unset.
    #[must_use]
    pub fn effective_homepage(&self) -> Option<&str> {
        self.homepage.as_deref().or(self.repository.as_deref())
    }

    /// Documentation URL, falling back to the repository URL when unset.
    #[must_use]
    pub fn effective_documentation(&self) -> Option<&str> {
        self.documentation.as_deref().or(self.repository.as_deref())
    }
}

/// The `[workspace]` section of `wado.toml`.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub members: Vec<String>,
    pub dependencies: IndexMap<String, Dependency>,
    pub dev_dependencies: IndexMap<String, Dependency>,
    /// The `[workspace.package]` table: metadata members inherit.
    pub package: Option<WorkspacePackage>,
}

/// The `[workspace.package]` table: package metadata shared by members.
/// `version`/`repository`/`namespace` are force-inherited; the rest are
/// overridable defaults. See [`resolve_member`].
#[derive(Debug, Clone, Default)]
pub struct WorkspacePackage {
    pub version: Option<String>,
    pub repository: Option<String>,
    pub namespace: Option<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub authors: Vec<String>,
    pub wado_version: Option<String>,
    /// Unknown keys in `[workspace.package]` (typos, or non-inheritable fields).
    /// Reported as warnings.
    pub unknown_fields: Vec<String>,
}

/// A single dependency declaration.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub source: DependencySource,
}

/// The source of a dependency.
#[derive(Debug, Clone)]
pub enum DependencySource {
    /// Git repository dependency.
    Git { url: String, pin: GitPin },
    /// Registry dependency.
    Registry {
        /// Registry alias name. `None` means use `"default"`.
        registry: Option<String>,
        /// Package identity in `namespace:name` format.
        package: String,
        /// Version specifier (e.g., `"^1.0.0"`).
        version: String,
    },
    /// Local path dependency.
    Path {
        path: String,
        /// Optional fallback source for publishing.
        publish_source: Option<Box<DependencySource>>,
    },
    /// Inherit from workspace.
    Workspace,
}

/// How a git dependency is pinned.
#[derive(Debug, Clone)]
pub enum GitPin {
    /// Semver range on git tags (e.g., `"^1.0.0"`).
    Version(String),
    /// Exact git ref (tag, branch, or commit SHA).
    Ref(String),
}

impl FromStr for Manifest {
    type Err = ManifestError;

    /// Parse a `wado.toml` string into a `Manifest`.
    ///
    /// This performs both TOML parsing and semantic validation.
    fn from_str(toml_str: &str) -> Result<Self, Self::Err> {
        let raw: RawManifest = toml::from_str::<RawManifest>(toml_str)
            .map_err(|e| ManifestError::Toml(e.to_string()))?;
        let manifest = convert_raw(raw)?;
        validate::validate(&manifest)?;
        Ok(manifest)
    }
}

/// Errors from manifest parsing and validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// TOML syntax error.
    Toml(String),
    /// Name or namespace format violation.
    InvalidName {
        field: String,
        value: String,
        reason: String,
    },
    /// Missing required field.
    MissingField { section: String, field: String },
    /// Dependency has conflicting source types.
    ConflictingSource { dep_name: String, message: String },
    /// Git dependency needs exactly one of `version` or `ref`.
    GitVersionRefConflict { dep_name: String },
    /// Bare version without prefix.
    BareVersion { dep_name: String, version: String },
    /// Invalid version string.
    InvalidVersion {
        context: String,
        version: String,
        reason: String,
    },
    /// Registry dependency without a default registry defined.
    NoDefaultRegistry { dep_name: String },
    /// `[package]` set both `license` and `license-file` (mutually exclusive).
    ConflictingLicense,
    /// `[package].license` is not a valid SPDX license expression.
    InvalidLicense { value: String, reason: String },
    /// `[package].wado-version` is not a valid semver requirement.
    InvalidWadoVersion { value: String, reason: String },
    /// A workspace member set a field that is force-inherited from
    /// `[workspace.package]` (`version`, `repository`, or `namespace`).
    WorkspaceFieldOverride { field: String },
    /// `[workspace.package]` set both `license` and `license-file`.
    WorkspaceConflictingLicense,
    /// `[workspace.package].license` is not a valid SPDX expression.
    WorkspaceInvalidLicense { value: String, reason: String },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Toml(msg) => write!(f, "TOML parse error: {msg}"),
            ManifestError::InvalidName {
                field,
                value,
                reason,
            } => write!(f, "invalid {field} {value:?}: {reason}"),
            ManifestError::MissingField { section, field } => {
                write!(f, "missing required field `{field}` in [{section}]")
            }
            ManifestError::ConflictingSource { dep_name, message } => {
                write!(f, "dependency {dep_name:?}: {message}")
            }
            ManifestError::GitVersionRefConflict { dep_name } => {
                write!(
                    f,
                    "dependency {dep_name:?}: git dependency must have exactly one of `version` or `ref`"
                )
            }
            ManifestError::BareVersion { dep_name, version } => {
                write!(
                    f,
                    "dependency {dep_name:?}: bare version {version:?} requires explicit prefix (^, ~, or =)"
                )
            }
            ManifestError::InvalidVersion {
                context,
                version,
                reason,
            } => write!(f, "{context}: invalid version {version:?}: {reason}"),
            ManifestError::NoDefaultRegistry { dep_name } => {
                write!(
                    f,
                    "dependency {dep_name:?}: registry dependency requires [registries].default"
                )
            }
            ManifestError::ConflictingLicense => write!(
                f,
                "[package]: `license` and `license-file` are mutually exclusive"
            ),
            ManifestError::InvalidLicense { value, reason } => write!(
                f,
                "[package].license {value:?} is not a valid SPDX expression: {reason}"
            ),
            ManifestError::InvalidWadoVersion { value, reason } => write!(
                f,
                "[package].wado-version {value:?} is not a valid version requirement: {reason}"
            ),
            ManifestError::WorkspaceFieldOverride { field } => write!(
                f,
                "[package].{field} is inherited from [workspace.package]; remove it from this member"
            ),
            ManifestError::WorkspaceConflictingLicense => write!(
                f,
                "[workspace.package]: `license` and `license-file` are mutually exclusive"
            ),
            ManifestError::WorkspaceInvalidLicense { value, reason } => write!(
                f,
                "[workspace.package].license {value:?} is not a valid SPDX expression: {reason}"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

// --- Raw serde types for TOML deserialization ---

#[derive(Deserialize)]
struct RawManifest {
    package: Option<RawPackage>,
    world: Option<IndexMap<String, String>>,
    registries: Option<IndexMap<String, String>>,
    dependencies: Option<IndexMap<String, RawDependency>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<IndexMap<String, RawDependency>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<IndexMap<String, RawDependency>>,
    workspace: Option<RawWorkspace>,
    test: Option<RawTestSettings>,
    format: Option<RawFormatSettings>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

fn unknown_keys(captured: &BTreeMap<String, toml::Value>) -> Vec<String> {
    captured.keys().cloned().collect()
}

#[derive(Deserialize)]
struct RawTestSettings {
    exclude: Option<Vec<String>>,
    include: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawFormatSettings {
    exclude: Option<Vec<String>>,
    include: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawPackage {
    namespace: Option<String>,
    name: Option<String>,
    version: Option<String>,
    lib: Option<String>,
    description: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    #[serde(rename = "repository-directory")]
    repository_directory: Option<String>,
    documentation: Option<String>,
    license: Option<String>,
    #[serde(rename = "license-file")]
    license_file: Option<String>,
    authors: Option<Vec<String>>,
    #[serde(rename = "wado-version")]
    wado_version: Option<String>,
    publish: Option<bool>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
struct RawWorkspace {
    members: Option<Vec<String>>,
    package: Option<RawWorkspacePackage>,
    dependencies: Option<IndexMap<String, RawDependency>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<IndexMap<String, RawDependency>>,
}

/// The `[workspace.package]` table: shared metadata inherited by members. Only
/// the inheritable fields are accepted; other keys are reported as warnings.
#[derive(Deserialize, Default)]
struct RawWorkspacePackage {
    version: Option<String>,
    repository: Option<String>,
    namespace: Option<String>,
    license: Option<String>,
    #[serde(rename = "license-file")]
    license_file: Option<String>,
    authors: Option<Vec<String>>,
    #[serde(rename = "wado-version")]
    wado_version: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
struct RawDependency {
    git: Option<String>,
    version: Option<String>,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    registry: Option<String>,
    package: Option<String>,
    path: Option<String>,
    workspace: Option<bool>,
}

fn convert_raw(raw: RawManifest) -> Result<Manifest, ManifestError> {
    let unknown_sections = unknown_keys(&raw.unknown);
    let package = raw.package.map(convert_package).transpose()?;
    let world = raw.world.unwrap_or_default();
    let registries = raw.registries.unwrap_or_default();
    let dependencies = convert_deps(raw.dependencies.unwrap_or_default())?;
    let dev_dependencies = convert_deps(raw.dev_dependencies.unwrap_or_default())?;
    let build_dependencies = convert_deps(raw.build_dependencies.unwrap_or_default())?;
    let workspace = raw.workspace.map(convert_workspace).transpose()?;
    let test = raw.test.map(convert_test).unwrap_or_default();
    let format = raw.format.map(convert_format).unwrap_or_default();

    Ok(Manifest {
        package,
        world,
        registries,
        dependencies,
        dev_dependencies,
        build_dependencies,
        workspace,
        test,
        format,
        unknown_sections,
        inherited_unknown_fields: Vec::new(),
    })
}

/// Parse a workspace member's manifest, inheriting metadata from the workspace
/// root's `[workspace.package]`.
///
/// `version`/`repository`/`namespace` are force-inherited: if the member sets
/// one (and the workspace defines it) it is a [`ManifestError::WorkspaceFieldOverride`].
/// `license`/`license-file`/`authors`/`wado-version` are defaults the member may
/// override. The merge runs before conversion, so required fields (e.g.
/// `version`) may be supplied by the workspace.
///
/// # Errors
/// Propagates TOML, inheritance, and validation errors for the merged member,
/// including problems in the root's `[workspace.package]` itself.
pub fn resolve_member(member_toml: &str, root_toml: &str) -> Result<Manifest, ManifestError> {
    let mut member_raw: RawManifest =
        toml::from_str(member_toml).map_err(|e| ManifestError::Toml(e.to_string()))?;
    let root_raw: RawManifest =
        toml::from_str(root_toml).map_err(|e| ManifestError::Toml(e.to_string()))?;

    let ws_pkg = root_raw
        .workspace
        .and_then(|w| w.package)
        .map(convert_workspace_package);
    let inherited_unknown_fields = ws_pkg
        .as_ref()
        .map(|p| p.unknown_fields.clone())
        .unwrap_or_default();

    if let (Some(pkg), Some(ws)) = (member_raw.package.as_mut(), ws_pkg.as_ref()) {
        validate::validate_workspace_package(ws)?;
        inherit_workspace_package(pkg, ws)?;
    }

    let mut manifest = convert_raw(member_raw)?;
    manifest.inherited_unknown_fields = inherited_unknown_fields;
    validate::validate(&manifest)?;
    Ok(manifest)
}

fn inherit_workspace_package(
    pkg: &mut RawPackage,
    ws: &WorkspacePackage,
) -> Result<(), ManifestError> {
    inherit_forced(&mut pkg.version, ws.version.as_ref(), "version")?;
    inherit_forced(&mut pkg.repository, ws.repository.as_ref(), "repository")?;
    inherit_forced(&mut pkg.namespace, ws.namespace.as_ref(), "namespace")?;
    if pkg.license.is_none() && pkg.license_file.is_none() {
        pkg.license.clone_from(&ws.license);
        pkg.license_file.clone_from(&ws.license_file);
    }
    if pkg.authors.is_none() && !ws.authors.is_empty() {
        pkg.authors = Some(ws.authors.clone());
    }
    if pkg.wado_version.is_none() {
        pkg.wado_version.clone_from(&ws.wado_version);
    }
    Ok(())
}

fn inherit_forced(
    field: &mut Option<String>,
    ws_value: Option<&String>,
    name: &'static str,
) -> Result<(), ManifestError> {
    if let Some(ws_value) = ws_value {
        if field.is_some() {
            return Err(ManifestError::WorkspaceFieldOverride {
                field: name.to_string(),
            });
        }
        *field = Some(ws_value.clone());
    }
    Ok(())
}

fn convert_test(raw: RawTestSettings) -> TestSettings {
    TestSettings {
        exclude: raw.exclude.unwrap_or_default(),
        include: raw.include.unwrap_or_default(),
    }
}

fn convert_format(raw: RawFormatSettings) -> FormatSettings {
    FormatSettings {
        exclude: raw.exclude.unwrap_or_default(),
        include: raw.include.unwrap_or_default(),
    }
}

fn convert_package(raw: RawPackage) -> Result<Package, ManifestError> {
    let name = raw.name.ok_or_else(|| ManifestError::MissingField {
        section: "package".to_string(),
        field: "name".to_string(),
    })?;
    let version = raw.version.ok_or_else(|| ManifestError::MissingField {
        section: "package".to_string(),
        field: "version".to_string(),
    })?;
    Ok(Package {
        namespace: raw.namespace,
        name,
        version,
        lib: raw.lib,
        description: raw.description,
        homepage: raw.homepage,
        repository: raw.repository,
        repository_directory: raw.repository_directory,
        documentation: raw.documentation,
        license: raw.license,
        license_file: raw.license_file,
        authors: raw.authors.unwrap_or_default(),
        wado_version: raw.wado_version,
        publish: raw.publish.unwrap_or(true),
        unknown_fields: unknown_keys(&raw.unknown),
    })
}

fn convert_workspace(raw: RawWorkspace) -> Result<Workspace, ManifestError> {
    let members = raw.members.ok_or_else(|| ManifestError::MissingField {
        section: "workspace".to_string(),
        field: "members".to_string(),
    })?;
    let dependencies = convert_deps(raw.dependencies.unwrap_or_default())?;
    let dev_dependencies = convert_deps(raw.dev_dependencies.unwrap_or_default())?;
    Ok(Workspace {
        members,
        dependencies,
        dev_dependencies,
        package: raw.package.map(convert_workspace_package),
    })
}

fn convert_workspace_package(raw: RawWorkspacePackage) -> WorkspacePackage {
    WorkspacePackage {
        version: raw.version,
        repository: raw.repository,
        namespace: raw.namespace,
        license: raw.license,
        license_file: raw.license_file,
        authors: raw.authors.unwrap_or_default(),
        wado_version: raw.wado_version,
        unknown_fields: unknown_keys(&raw.unknown),
    }
}

fn convert_deps(
    raw: IndexMap<String, RawDependency>,
) -> Result<IndexMap<String, Dependency>, ManifestError> {
    let mut result = IndexMap::new();
    for (name, raw_dep) in raw {
        let dep = convert_dep(&name, raw_dep)?;
        result.insert(name, dep);
    }
    Ok(result)
}

fn convert_dep(name: &str, raw: RawDependency) -> Result<Dependency, ManifestError> {
    // Count primary source types
    let has_git = raw.git.is_some();
    let has_package = raw.package.is_some();
    let has_path = raw.path.is_some();
    let has_workspace = raw.workspace == Some(true);

    // `workspace = true` is exclusive
    if has_workspace {
        if has_git || has_package || has_path {
            return Err(ManifestError::ConflictingSource {
                dep_name: name.to_string(),
                message: "`workspace = true` cannot be combined with other source types"
                    .to_string(),
            });
        }
        return Ok(Dependency {
            source: DependencySource::Workspace,
        });
    }

    // Path can be combined with git or registry for publishing
    if has_path {
        let publish_source = if has_git {
            Some(Box::new(build_git_source(name, &raw)?))
        } else if has_package {
            let package = raw.package.clone().expect("has_package checked");
            Some(Box::new(build_registry_source(name, package, &raw)?))
        } else {
            None
        };
        return Ok(Dependency {
            source: DependencySource::Path {
                path: raw.path.unwrap(),
                publish_source,
            },
        });
    }

    if has_git && has_package {
        return Err(ManifestError::ConflictingSource {
            dep_name: name.to_string(),
            message: "cannot have both `git` and `package` sources".to_string(),
        });
    }

    if has_git {
        return Ok(Dependency {
            source: build_git_source(name, &raw)?,
        });
    }

    if has_package {
        return Ok(Dependency {
            source: build_registry_source(name, raw.package.clone().expect("has_package"), &raw)?,
        });
    }

    // Open coordinate: the key is its own registry identity, `package` omitted.
    if is_open_coordinate(name) {
        return Ok(Dependency {
            source: build_registry_source(name, name.to_string(), &raw)?,
        });
    }

    // Registry-shaped but the identity is unknown (non-coordinate key, no `package`).
    if raw.registry.is_some() || raw.version.is_some() {
        return Err(ManifestError::ConflictingSource {
            dep_name: name.to_string(),
            message: "registry dependency needs a coordinate key `ns:pkg` or a `package = \"ns:pkg\"` field"
                .to_string(),
        });
    }

    Err(ManifestError::ConflictingSource {
        dep_name: name.to_string(),
        message: "dependency must specify one of: `git`, `package`, `path`, or `workspace = true`"
            .to_string(),
    })
}

// Normalized rendering for `deps_hash`: an omitted registry renders as `default`.
fn source_fingerprint(source: &DependencySource) -> String {
    match source {
        DependencySource::Registry {
            registry,
            package,
            version,
        } => format!(
            "registry|{}|{package}|{version}",
            registry.as_deref().unwrap_or("default")
        ),
        DependencySource::Git { url, pin } => match pin {
            GitPin::Version(v) => format!("git|{url}|version|{v}"),
            GitPin::Ref(r) => format!("git|{url}|ref|{r}"),
        },
        DependencySource::Path {
            path,
            publish_source,
        } => {
            let pubsrc = publish_source
                .as_deref()
                .map(source_fingerprint)
                .unwrap_or_default();
            format!("path|{path}|{pubsrc}")
        }
        DependencySource::Workspace => "workspace".to_string(),
    }
}

// An open coordinate `ns:pkg` (two non-empty, non-reserved segments) is its own
// registry identity, so `package` may be omitted.
fn is_open_coordinate(key: &str) -> bool {
    match key.split_once(':') {
        Some((ns, pkg)) => {
            !ns.is_empty()
                && !pkg.is_empty()
                && !pkg.contains(':')
                && !matches!(ns, "wasi" | "core" | "lib")
        }
        None => false,
    }
}

fn build_git_source(name: &str, raw: &RawDependency) -> Result<DependencySource, ManifestError> {
    let url = raw.git.clone().expect("caller checked git is Some");

    let has_version = raw.version.is_some();
    let has_ref = raw.git_ref.is_some();

    if has_version == has_ref {
        return Err(ManifestError::GitVersionRefConflict {
            dep_name: name.to_string(),
        });
    }

    let pin = if let Some(version) = &raw.version {
        // Validate version specifier format
        VersionSpecifier::parse(version).map_err(|e| ManifestError::InvalidVersion {
            context: format!("dependency {name:?}"),
            version: version.clone(),
            reason: e.to_string(),
        })?;
        GitPin::Version(version.clone())
    } else {
        GitPin::Ref(raw.git_ref.clone().unwrap())
    };

    Ok(DependencySource::Git { url, pin })
}

// `package` is the resolved identity: the `package` field, or the key itself
// for an open coordinate.
fn build_registry_source(
    name: &str,
    package: String,
    raw: &RawDependency,
) -> Result<DependencySource, ManifestError> {
    let version = raw
        .version
        .clone()
        .ok_or_else(|| ManifestError::MissingField {
            section: format!("dependencies.{name}"),
            field: "version".to_string(),
        })?;

    // Validate version specifier format
    VersionSpecifier::parse(&version).map_err(|e| ManifestError::InvalidVersion {
        context: format!("dependency {name:?}"),
        version: version.clone(),
        reason: e.to_string(),
    })?;

    Ok(DependencySource::Registry {
        registry: raw.registry.clone(),
        package,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let toml = r#"
[package]
name = "my-app"
version = "0.1.0"

[world]
"wasi:cli/command" = "src/main.wado"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let pkg = m.package.as_ref().unwrap();
        assert_eq!(pkg.name, "my-app");
        assert_eq!(pkg.version, "0.1.0");
        assert_eq!(m.world_entry("wasi:cli/command"), Some("src/main.wado"));
        assert!(pkg.namespace.is_none());
    }

    #[test]
    fn parse_git_dep_with_version() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[dependencies]
router = { git = "https://github.com/user/router.git", version = "^1.0.0" }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let dep = &m.dependencies["router"];
        assert!(matches!(
            &dep.source,
            DependencySource::Git {
                url,
                pin: GitPin::Version(v),
            } if url == "https://github.com/user/router.git" && v == "^1.0.0"
        ));
    }

    #[test]
    fn parse_open_coordinate_registry_dep() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[registries]
default = "https://wa.dev"

[dependencies]
"mizchi:brotli" = { version = "^0.2.0" }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let dep = &m.dependencies["mizchi:brotli"];
        assert!(matches!(
            &dep.source,
            DependencySource::Registry {
                registry: None,
                package,
                version,
            } if package == "mizchi:brotli" && version == "^0.2.0"
        ));
    }

    #[test]
    fn bare_dependency_key_warns_but_parses() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
router = { git = "https://github.com/user/router.git", ref = "main" }
"mizchi:brotli" = { version = "^0.2.0" }
"lib:shared" = { path = "../shared" }

[registries]
default = "https://wa.dev"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let warnings = m.warnings();
        assert_eq!(warnings.len(), 1, "got {warnings:?}");
        assert!(matches!(
            &warnings[0],
            ManifestWarning::BareDependencyKey { key } if key == "router"
        ));
    }

    #[test]
    fn deps_hash_normalizes_default_registry() {
        let implicit = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "oci://ghcr.io/acme"

[dependencies]
"ns:pkg" = { version = "^1.0.0" }
"#
        .parse::<Manifest>()
        .unwrap();
        let explicit = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "oci://ghcr.io/acme"

[dependencies]
"ns:pkg" = { registry = "default", package = "ns:pkg", version = "^1.0.0" }
"#
        .parse::<Manifest>()
        .unwrap();
        assert_eq!(implicit.deps_hash(), explicit.deps_hash());
        assert!(implicit.deps_hash().starts_with("sha256:"));
    }

    #[test]
    fn deps_hash_changes_with_version() {
        let mk = |ver: &str| -> Manifest {
            format!(
                "[package]\nname=\"a\"\nversion=\"0.1.0\"\n\n[registries]\ndefault=\"oci://ghcr.io/acme\"\n\n[dependencies]\n\"ns:pkg\" = {{ version = \"{ver}\" }}\n"
            )
            .parse()
            .unwrap()
        };
        assert_ne!(mk("^1.0.0").deps_hash(), mk("^2.0.0").deps_hash());
    }

    #[test]
    fn parse_lib_nickname_path_dep() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"lib:shared" = { path = "../shared" }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let dep = &m.dependencies["lib:shared"];
        assert!(matches!(
            &dep.source,
            DependencySource::Path { path, publish_source: None } if path == "../shared"
        ));
    }

    #[test]
    fn parse_open_coordinate_with_explicit_registry() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
custom = "https://registry.example.com"

[dependencies]
"mizchi:brotli" = { registry = "custom", version = "^0.2.0" }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let dep = &m.dependencies["mizchi:brotli"];
        assert!(matches!(
            &dep.source,
            DependencySource::Registry {
                registry: Some(reg),
                package,
                version,
            } if reg == "custom" && package == "mizchi:brotli" && version == "^0.2.0"
        ));
    }

    #[test]
    fn open_coordinate_missing_version_reports_missing_field() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "oci://ghcr.io/acme"

[dependencies]
"mizchi:brotli" = { registry = "default" }
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(
            matches!(&err, ManifestError::MissingField { field, .. } if field == "version"),
            "{err:?}"
        );
    }

    #[test]
    fn registry_shaped_bare_key_reports_helpful_error() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "oci://ghcr.io/acme"

[dependencies]
foo = { version = "^1.0.0" }
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(
            matches!(&err, ManifestError::ConflictingSource { message, .. } if message.contains("coordinate key")),
            "{err:?}"
        );
    }

    #[test]
    fn open_coordinate_without_default_registry_rejected() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
"mizchi:brotli" = { version = "^0.2.0" }
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(
            matches!(err, ManifestError::NoDefaultRegistry { .. }),
            "expected NoDefaultRegistry, got {err:?}"
        );
    }

    #[test]
    fn parse_git_dep_with_ref() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[dependencies]
router = { git = "https://github.com/user/router.git", ref = "main" }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let dep = &m.dependencies["router"];
        assert!(matches!(
            &dep.source,
            DependencySource::Git {
                pin: GitPin::Ref(r),
                ..
            } if r == "main"
        ));
    }

    #[test]
    fn git_both_version_and_ref_rejected() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[dependencies]
router = { git = "https://example.com/r.git", version = "^1.0.0", ref = "main" }
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(matches!(err, ManifestError::GitVersionRefConflict { .. }));
    }

    #[test]
    fn bare_version_rejected() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[dependencies]
regex = { package = "docs:regex", version = "1.0.0" }
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(
            matches!(err, ManifestError::InvalidVersion { .. }),
            "expected InvalidVersion, got {err:?}"
        );
    }

    #[test]
    fn path_with_publish_source() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[dependencies]
shared = { path = "../shared", package = "myorg:shared", version = "^0.1.0" }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let dep = &m.dependencies["shared"];
        match &dep.source {
            DependencySource::Path {
                path,
                publish_source,
            } => {
                assert_eq!(path, "../shared");
                assert!(publish_source.is_some());
                assert!(matches!(
                    publish_source.as_deref(),
                    Some(DependencySource::Registry { .. })
                ));
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn parse_test_exclude() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[test]
exclude = ["wado-compiler/tests/**", "vendor/**"]
"#;
        let m = toml.parse::<Manifest>().unwrap();
        assert_eq!(m.test.exclude, vec!["wado-compiler/tests/**", "vendor/**"]);
    }

    #[test]
    fn test_section_defaults_when_omitted() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        assert!(m.test.exclude.is_empty());
        assert!(m.test.include.is_empty());
    }

    #[test]
    fn parse_test_include() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[test]
exclude = ["lib/core/prelude/**"]
include = ["lib/**/*_test.wado"]
"#;
        let m = toml.parse::<Manifest>().unwrap();
        assert_eq!(m.test.exclude, vec!["lib/core/prelude/**"]);
        assert_eq!(m.test.include, vec!["lib/**/*_test.wado"]);
    }

    #[test]
    fn parse_format_exclude_include() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"

[format]
exclude = ["wado-compiler/tests/fixtures/**"]
include = ["wado-compiler/tests/fixtures/keepme.wado"]
"#;
        let m = toml.parse::<Manifest>().unwrap();
        assert_eq!(m.format.exclude, vec!["wado-compiler/tests/fixtures/**"]);
        assert_eq!(
            m.format.include,
            vec!["wado-compiler/tests/fixtures/keepme.wado"]
        );
    }

    #[test]
    fn format_section_defaults_when_omitted() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[world]
"wasi:cli/command" = "main.wado"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        assert!(m.format.exclude.is_empty());
        assert!(m.format.include.is_empty());
    }

    #[test]
    fn workspace_dep() {
        let toml = r#"
[package]
name = "member"
version = "0.1.0"
lib = "src/lib.wado"

[dependencies]
json = { workspace = true }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        assert!(matches!(
            m.dependencies["json"].source,
            DependencySource::Workspace
        ));
    }

    #[test]
    fn parse_package_metadata() {
        let toml = r#"
[package]
namespace = "myorg"
name = "my-app"
version = "0.1.0"
description = "A fast widget toolkit"
homepage = "https://wado-lang.org"
repository = "https://github.com/myorg/my-app"
repository-directory = "packages/foo"
documentation = "https://docs.wado-lang.org"
license = "MIT OR Apache-2.0"
authors = ["Alice <a@example.com>", "Bob"]
wado-version = ">=0.5"
publish = false
"#;
        let pkg = toml.parse::<Manifest>().unwrap().package.unwrap();
        assert_eq!(pkg.description.as_deref(), Some("A fast widget toolkit"));
        assert_eq!(pkg.homepage.as_deref(), Some("https://wado-lang.org"));
        assert_eq!(
            pkg.repository.as_deref(),
            Some("https://github.com/myorg/my-app")
        );
        assert_eq!(pkg.repository_directory.as_deref(), Some("packages/foo"));
        assert_eq!(
            pkg.documentation.as_deref(),
            Some("https://docs.wado-lang.org")
        );
        assert_eq!(pkg.license.as_deref(), Some("MIT OR Apache-2.0"));
        assert_eq!(pkg.authors, vec!["Alice <a@example.com>", "Bob"]);
        assert_eq!(pkg.wado_version.as_deref(), Some(">=0.5"));
        assert!(!pkg.publish);
    }

    #[test]
    fn metadata_defaults_when_omitted() {
        let toml = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        let pkg = toml.parse::<Manifest>().unwrap().package.unwrap();
        assert!(pkg.description.is_none());
        assert!(pkg.authors.is_empty());
        assert!(pkg.wado_version.is_none());
        assert!(pkg.publish, "publish defaults to true");
        assert!(pkg.repository_directory.is_none());
    }

    #[test]
    fn homepage_and_documentation_fall_back_to_repository() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
repository = "https://github.com/org/app"
"#;
        let pkg = toml.parse::<Manifest>().unwrap().package.unwrap();
        assert_eq!(
            pkg.effective_homepage(),
            Some("https://github.com/org/app")
        );
        assert_eq!(
            pkg.effective_documentation(),
            Some("https://github.com/org/app")
        );
    }

    #[test]
    fn explicit_homepage_overrides_repository_fallback() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
homepage = "https://app.example"
repository = "https://github.com/org/app"
"#;
        let pkg = toml.parse::<Manifest>().unwrap().package.unwrap();
        assert_eq!(pkg.effective_homepage(), Some("https://app.example"));
    }

    #[test]
    fn unknown_package_field_warns_but_parses() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
descshunption = "typo"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let warnings = m.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                ManifestWarning::UnknownField { section: Some(s), field }
                    if s == "package" && field == "descshunption"
            )),
            "got {warnings:?}"
        );
    }

    #[test]
    fn unknown_top_level_section_warns_but_parses() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[dependancies]
"ns:x" = { version = "^1.0.0" }
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let warnings = m.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                ManifestWarning::UnknownField { section: None, field } if field == "dependancies"
            )),
            "got {warnings:?}"
        );
    }

    #[test]
    fn license_and_license_file_conflict_rejected() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
license = "MIT"
license-file = "LICENSE"
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(matches!(err, ManifestError::ConflictingLicense), "{err:?}");
    }

    #[test]
    fn invalid_wado_version_rejected() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
wado-version = "not a req"
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(
            matches!(err, ManifestError::InvalidWadoVersion { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn valid_spdx_license_accepted() {
        for license in ["MIT", "MIT OR Apache-2.0", "Apache-2.0 WITH LLVM-exception"] {
            let toml = format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\nlicense = \"{license}\"\n");
            assert!(
                toml.parse::<Manifest>().is_ok(),
                "expected {license:?} to be accepted"
            );
        }
    }

    #[test]
    fn license_ref_for_nonstandard_license_accepted() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
license = "LicenseRef-Commercial"
"#;
        assert!(toml.parse::<Manifest>().is_ok());
    }

    #[test]
    fn invalid_spdx_license_rejected() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
license = "Definitely Not A License"
"#;
        let err = toml.parse::<Manifest>().unwrap_err();
        assert!(matches!(err, ManifestError::InvalidLicense { .. }), "{err:?}");
    }

    const ROOT_WS: &str = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.2.0"
repository = "https://github.com/org/monorepo"
namespace = "org"
license = "MIT"
authors = ["Alice <a@example.com>"]
"#;

    #[test]
    fn member_inherits_workspace_metadata() {
        let member = r#"
[package]
name = "core"
description = "Shared core"
lib = "src/lib.wado"
"#;
        let pkg = resolve_member(member, ROOT_WS).unwrap().package.unwrap();
        assert_eq!(pkg.version, "0.2.0");
        assert_eq!(pkg.repository.as_deref(), Some("https://github.com/org/monorepo"));
        assert_eq!(pkg.namespace.as_deref(), Some("org"));
        assert_eq!(pkg.license.as_deref(), Some("MIT"));
        assert_eq!(pkg.authors, vec!["Alice <a@example.com>"]);
        assert_eq!(pkg.name, "core");
        assert_eq!(pkg.description.as_deref(), Some("Shared core"));
    }

    #[test]
    fn member_overriding_forced_field_is_error() {
        for (field, line) in [
            ("version", "version = \"9.9.9\""),
            ("repository", "repository = \"https://example.com/other\""),
            ("namespace", "namespace = \"other\""),
        ] {
            let member = format!("[package]\nname = \"core\"\nlib = \"src/lib.wado\"\n{line}\n");
            let err = resolve_member(&member, ROOT_WS).unwrap_err();
            assert!(
                matches!(&err, ManifestError::WorkspaceFieldOverride { field: f } if f == field),
                "field {field}: {err:?}"
            );
        }
    }

    #[test]
    fn member_may_override_default_fields() {
        let member = r#"
[package]
name = "core"
lib = "src/lib.wado"
license = "Apache-2.0"
authors = ["Bob"]
"#;
        let pkg = resolve_member(member, ROOT_WS).unwrap().package.unwrap();
        assert_eq!(pkg.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(pkg.authors, vec!["Bob"]);
        assert_eq!(pkg.version, "0.2.0");
    }

    #[test]
    fn member_license_file_overrides_inherited_license_slot() {
        let member = r#"
[package]
name = "core"
lib = "src/lib.wado"
license-file = "LICENSE-CUSTOM"
"#;
        let pkg = resolve_member(member, ROOT_WS).unwrap().package.unwrap();
        assert_eq!(pkg.license_file.as_deref(), Some("LICENSE-CUSTOM"));
        assert!(pkg.license.is_none(), "inherited license must not coexist");
    }

    #[test]
    fn member_missing_version_without_workspace_version_errors() {
        let root = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
repository = "https://github.com/org/monorepo"
"#;
        let member = "[package]\nname = \"core\"\nlib = \"src/lib.wado\"\n";
        let err = resolve_member(member, root).unwrap_err();
        assert!(
            matches!(&err, ManifestError::MissingField { field, .. } if field == "version"),
            "{err:?}"
        );
    }

    #[test]
    fn member_resolution_surfaces_root_workspace_package_typo() {
        let root = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.1.0"
licence = "MIT"
"#;
        let member = "[package]\nname = \"core\"\nlib = \"src/lib.wado\"\n";
        let m = resolve_member(member, root).unwrap();
        let warnings = m.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                ManifestWarning::UnknownField { section: Some(s), field }
                    if s == "workspace.package" && field == "licence"
            )),
            "got {warnings:?}"
        );
    }

    #[test]
    fn workspace_package_conflicting_license_attributed_to_workspace() {
        let root = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.1.0"
license = "MIT"
license-file = "LICENSE"
"#;
        let member = "[package]\nname = \"core\"\nlib = \"src/lib.wado\"\n";
        let err = resolve_member(member, root).unwrap_err();
        assert!(
            matches!(err, ManifestError::WorkspaceConflictingLicense),
            "{err:?}"
        );
        let err = root.parse::<Manifest>().unwrap_err();
        assert!(
            matches!(err, ManifestError::WorkspaceConflictingLicense),
            "{err:?}"
        );
    }

    #[test]
    fn workspace_package_invalid_spdx_rejected() {
        let root = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.1.0"
license = "Not A License"
"#;
        let err = root.parse::<Manifest>().unwrap_err();
        assert!(
            matches!(err, ManifestError::WorkspaceInvalidLicense { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn unknown_workspace_package_field_warns() {
        let toml = r#"
[workspace]
members = ["packages/*"]

[workspace.package]
version = "0.1.0"
description = "not inheritable"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let warnings = m.warnings();
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                ManifestWarning::UnknownField { section: Some(s), field }
                    if s == "workspace.package" && field == "description"
            )),
            "got {warnings:?}"
        );
    }
}
