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
    pub registries: IndexMap<String, String>,
    pub dependencies: IndexMap<String, Dependency>,
    pub dev_dependencies: IndexMap<String, Dependency>,
    pub build_dependencies: IndexMap<String, Dependency>,
    pub workspace: Option<Workspace>,
    pub test: TestSettings,
}

/// The `[test]` section of `wado.toml`.
///
/// Controls how `wado test` discovers and runs `*.wado` files in the package.
#[derive(Debug, Clone, Default)]
pub struct TestSettings {
    /// Glob patterns (relative to the package root) for paths to exclude from
    /// test discovery. See WEP 2026-05-02.
    pub exclude: Vec<String>,
}

/// The `[package]` section of `wado.toml`.
#[derive(Debug, Clone)]
pub struct Package {
    pub namespace: Option<String>,
    pub name: String,
    pub version: String,
    pub command: Option<String>,
    pub service: Option<String>,
    pub lib: Option<String>,
    /// `generator = "<path>"` — entry file that exports the
    /// `core:kiln/generator` world. Presence marks the package as a Kiln
    /// generator; see WEP 2026-04-12.
    pub generator: Option<String>,
}

/// The `[workspace]` section of `wado.toml`.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub members: Vec<String>,
    pub dependencies: IndexMap<String, Dependency>,
    pub dev_dependencies: IndexMap<String, Dependency>,
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
        }
    }
}

impl std::error::Error for ManifestError {}

// --- Raw serde types for TOML deserialization ---

#[derive(Deserialize)]
struct RawManifest {
    package: Option<RawPackage>,
    registries: Option<IndexMap<String, String>>,
    dependencies: Option<IndexMap<String, RawDependency>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<IndexMap<String, RawDependency>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<IndexMap<String, RawDependency>>,
    workspace: Option<RawWorkspace>,
    test: Option<RawTestSettings>,
}

#[derive(Deserialize)]
struct RawTestSettings {
    exclude: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawPackage {
    namespace: Option<String>,
    name: Option<String>,
    version: Option<String>,
    command: Option<String>,
    service: Option<String>,
    lib: Option<String>,
    generator: Option<String>,
}

#[derive(Deserialize)]
struct RawWorkspace {
    members: Option<Vec<String>>,
    dependencies: Option<IndexMap<String, RawDependency>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<IndexMap<String, RawDependency>>,
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
    let package = raw.package.map(convert_package).transpose()?;
    let registries = raw.registries.unwrap_or_default();
    let dependencies = convert_deps(raw.dependencies.unwrap_or_default())?;
    let dev_dependencies = convert_deps(raw.dev_dependencies.unwrap_or_default())?;
    let build_dependencies = convert_deps(raw.build_dependencies.unwrap_or_default())?;
    let workspace = raw.workspace.map(convert_workspace).transpose()?;
    let test = raw.test.map(convert_test).unwrap_or_default();

    Ok(Manifest {
        package,
        registries,
        dependencies,
        dev_dependencies,
        build_dependencies,
        workspace,
        test,
    })
}

fn convert_test(raw: RawTestSettings) -> TestSettings {
    TestSettings {
        exclude: raw.exclude.unwrap_or_default(),
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
        command: raw.command,
        service: raw.service,
        lib: raw.lib,
        generator: raw.generator,
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
    })
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
            Some(Box::new(build_registry_source(name, &raw)?))
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
            source: build_registry_source(name, &raw)?,
        });
    }

    Err(ManifestError::ConflictingSource {
        dep_name: name.to_string(),
        message: "dependency must specify one of: `git`, `package`, `path`, or `workspace = true`"
            .to_string(),
    })
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

fn build_registry_source(
    name: &str,
    raw: &RawDependency,
) -> Result<DependencySource, ManifestError> {
    let package = raw.package.clone().expect("caller checked package is Some");
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
command = "src/main.wado"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        let pkg = m.package.unwrap();
        assert_eq!(pkg.name, "my-app");
        assert_eq!(pkg.version, "0.1.0");
        assert_eq!(pkg.command.as_deref(), Some("src/main.wado"));
        assert!(pkg.namespace.is_none());
    }

    #[test]
    fn parse_git_dep_with_version() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

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
    fn parse_git_dep_with_ref() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

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
command = "main.wado"

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
command = "main.wado"

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
command = "main.wado"

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
command = "main.wado"

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
command = "main.wado"
"#;
        let m = toml.parse::<Manifest>().unwrap();
        assert!(m.test.exclude.is_empty());
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
}
