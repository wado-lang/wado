use crate::manifest::{DependencySource, Manifest, ManifestError, WorkspacePackage};
use crate::version::{Version, VersionSpecifier};

/// Validate a parsed manifest for semantic consistency.
///
/// Called automatically by `Manifest::from_str`, but also available
/// separately for incremental validation (e.g., LSP).
pub fn validate(manifest: &Manifest) -> Result<(), ManifestError> {
    if let Some(pkg) = &manifest.package {
        validate_package(pkg)?;
    }
    if let Some(ws_pkg) = manifest.workspace.as_ref().and_then(|w| w.package.as_ref()) {
        validate_workspace_package(ws_pkg)?;
    }
    validate_dependencies(manifest)?;
    Ok(())
}

/// Validate the `[workspace.package]` table at its source, so problems are
/// attributed to the workspace root rather than to an inheriting member.
pub(crate) fn validate_workspace_package(p: &WorkspacePackage) -> Result<(), ManifestError> {
    if p.license.is_some() && p.license_file.is_some() {
        return Err(ManifestError::WorkspaceConflictingLicense);
    }
    if let Some(license) = &p.license {
        spdx::Expression::parse(license).map_err(|e| ManifestError::WorkspaceInvalidLicense {
            value: license.clone(),
            reason: e.to_string(),
        })?;
    }
    if let Some(version) = &p.version {
        Version::parse(version).map_err(|e| ManifestError::InvalidVersion {
            context: "workspace.package.version".to_string(),
            version: version.clone(),
            reason: e.to_string(),
        })?;
    }
    if let Some(namespace) = &p.namespace {
        validate_name("workspace.package.namespace", namespace)?;
    }
    if let Some(req) = &p.wado_version {
        semver::VersionReq::parse(req).map_err(|e| ManifestError::InvalidWadoVersion {
            value: req.clone(),
            reason: e.to_string(),
        })?;
    }
    Ok(())
}

fn validate_package(pkg: &crate::manifest::Package) -> Result<(), ManifestError> {
    validate_name("package.name", &pkg.name)?;
    if let Some(ns) = &pkg.namespace {
        validate_name("package.namespace", ns)?;
    }
    // Validate version is valid semver
    Version::parse(&pkg.version).map_err(|e| ManifestError::InvalidVersion {
        context: "package.version".to_string(),
        version: pkg.version.clone(),
        reason: e.to_string(),
    })?;
    if pkg.license.is_some() && pkg.license_file.is_some() {
        return Err(ManifestError::ConflictingLicense);
    }
    if let Some(license) = &pkg.license {
        spdx::Expression::parse(license).map_err(|e| ManifestError::InvalidLicense {
            value: license.clone(),
            reason: e.to_string(),
        })?;
    }
    if let Some(req) = &pkg.wado_version {
        semver::VersionReq::parse(req).map_err(|e| ManifestError::InvalidWadoVersion {
            value: req.clone(),
            reason: e.to_string(),
        })?;
    }
    Ok(())
}

/// Reasons a package is not ready to publish. Returned by
/// [`validate_for_publish`]; an empty result means the package is publishable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// No `[package]` section.
    NoPackage,
    /// `namespace` is absent, so the package has no registry identity.
    MissingNamespace,
    /// `publish = false` opts the package out.
    PublishDisabled,
    /// A required descriptive field (`description`, `repository`, `authors`) is
    /// missing.
    MissingMetadata { field: &'static str },
    /// Neither `license` nor `license-file` is set.
    MissingLicense,
    /// A shipped `path` dependency lacks a registry/git source for publishing.
    PathDependencyWithoutSource { dep_name: String },
    /// A shipped `workspace = true` dependency has no concrete source once the
    /// package is extracted from its workspace.
    WorkspaceDependency { dep_name: String },
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::NoPackage => write!(f, "no [package] section to publish"),
            PublishError::MissingNamespace => {
                write!(f, "[package].namespace is required to publish")
            }
            PublishError::PublishDisabled => {
                write!(f, "[package].publish = false opts out of publishing")
            }
            PublishError::MissingMetadata { field } => {
                write!(f, "[package].{field} is required to publish")
            }
            PublishError::MissingLicense => write!(
                f,
                "[package] requires `license` or `license-file` to publish"
            ),
            PublishError::PathDependencyWithoutSource { dep_name } => write!(
                f,
                "path dependency {dep_name:?} needs a registry or git source to publish"
            ),
            PublishError::WorkspaceDependency { dep_name } => write!(
                f,
                "workspace dependency {dep_name:?} must resolve to a concrete source to publish"
            ),
        }
    }
}

impl std::error::Error for PublishError {}

/// Check whether a manifest meets the requirements to publish, collecting every
/// problem rather than stopping at the first. An empty `Vec` means publishable.
///
/// Per WEP, a published package must carry its descriptive metadata
/// (`description`, `repository`, `authors`) and a license; `homepage` and
/// `documentation` fall back to `repository`, and `repository-directory` /
/// `wado-version` stay optional.
#[must_use]
pub fn validate_for_publish(manifest: &Manifest) -> Vec<PublishError> {
    let mut errors = Vec::new();
    let Some(pkg) = &manifest.package else {
        return vec![PublishError::NoPackage];
    };
    if !pkg.publish {
        errors.push(PublishError::PublishDisabled);
    }
    if pkg.namespace.is_none() {
        errors.push(PublishError::MissingNamespace);
    }
    if pkg.description.is_none() {
        errors.push(PublishError::MissingMetadata {
            field: "description",
        });
    }
    if pkg.repository.is_none() {
        errors.push(PublishError::MissingMetadata { field: "repository" });
    }
    if pkg.authors.is_empty() {
        errors.push(PublishError::MissingMetadata { field: "authors" });
    }
    if pkg.license.is_none() && pkg.license_file.is_none() {
        errors.push(PublishError::MissingLicense);
    }
    for (name, dep) in manifest.dependencies.iter().chain(&manifest.build_dependencies) {
        match &dep.source {
            DependencySource::Path {
                publish_source: None,
                ..
            } => errors.push(PublishError::PathDependencyWithoutSource {
                dep_name: name.clone(),
            }),
            DependencySource::Workspace => errors.push(PublishError::WorkspaceDependency {
                dep_name: name.clone(),
            }),
            DependencySource::Path { .. }
            | DependencySource::Git { .. }
            | DependencySource::Registry { .. } => {}
        }
    }
    errors
}

/// Validate that a name matches `[a-zA-Z0-9_-]+` and is 1-64 characters.
fn validate_name(field: &str, value: &str) -> Result<(), ManifestError> {
    if value.is_empty() {
        return Err(ManifestError::InvalidName {
            field: field.to_string(),
            value: value.to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    if value.len() > 64 {
        return Err(ManifestError::InvalidName {
            field: field.to_string(),
            value: value.to_string(),
            reason: "must be at most 64 characters".to_string(),
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ManifestError::InvalidName {
            field: field.to_string(),
            value: value.to_string(),
            reason: "must match [a-zA-Z0-9_-]+".to_string(),
        });
    }
    Ok(())
}

fn validate_dependencies(manifest: &Manifest) -> Result<(), ManifestError> {
    for (name, dep) in &manifest.dependencies {
        validate_dep_key(name)?;
        validate_dep_source(name, &dep.source, manifest, false)?;
    }
    for (name, dep) in &manifest.dev_dependencies {
        validate_dep_key(name)?;
        validate_dep_source(name, &dep.source, manifest, false)?;
    }
    for (name, dep) in &manifest.build_dependencies {
        validate_dep_key(name)?;
        validate_dep_source(name, &dep.source, manifest, false)?;
    }
    Ok(())
}

fn validate_dep_key(name: &str) -> Result<(), ManifestError> {
    // Validate each `:`-separated segment of a coordinate / `lib:` nickname key.
    for segment in name.split(':') {
        validate_name("dependency key", segment)?;
    }
    Ok(())
}

fn validate_dep_source(
    name: &str,
    source: &DependencySource,
    manifest: &Manifest,
    is_publish_source: bool,
) -> Result<(), ManifestError> {
    match source {
        DependencySource::Git { pin, .. } => {
            if let crate::manifest::GitPin::Version(v) = pin {
                validate_version_specifier(name, v)?;
            }
        }
        DependencySource::Registry {
            registry, version, ..
        } => {
            validate_version_specifier(name, version)?;
            // If no explicit registry, "default" must exist.
            // Skip this check for publish sources inside path deps — those are
            // only used at `wado publish` time, not during development.
            if !is_publish_source
                && registry.is_none()
                && !manifest.registries.contains_key("default")
            {
                return Err(ManifestError::NoDefaultRegistry {
                    dep_name: name.to_string(),
                });
            }
        }
        DependencySource::Path { publish_source, .. } => {
            if let Some(inner) = publish_source {
                validate_dep_source(name, inner, manifest, true)?;
            }
        }
        DependencySource::Workspace => {
            // Workspace dependency requires a workspace section to exist somewhere.
            // Full resolution is the consumer's responsibility (CLI resolves against
            // the workspace root's manifest). We skip validation here since this
            // manifest might be a member that doesn't contain [workspace] itself.
        }
    }
    Ok(())
}

fn validate_version_specifier(dep_name: &str, version: &str) -> Result<(), ManifestError> {
    VersionSpecifier::parse(version).map_err(|e| {
        // Distinguish bare version from other parse errors
        if matches!(e, crate::version::VersionError::BareVersion { .. }) {
            ManifestError::BareVersion {
                dep_name: dep_name.to_string(),
                version: version.to_string(),
            }
        } else {
            ManifestError::InvalidVersion {
                context: format!("dependency {dep_name:?}"),
                version: version.to_string(),
                reason: e.to_string(),
            }
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert!(validate_name("test", "my-app").is_ok());
        assert!(validate_name("test", "my_app").is_ok());
        assert!(validate_name("test", "App123").is_ok());
        assert!(validate_name("test", "a").is_ok());
    }

    #[test]
    fn invalid_names() {
        assert!(validate_name("test", "").is_err());
        assert!(validate_name("test", "my app").is_err());
        assert!(validate_name("test", "my.app").is_err());
        assert!(validate_name("test", "my/app").is_err());
        let long = "a".repeat(65);
        assert!(validate_name("test", &long).is_err());
    }

    #[test]
    fn max_length_name() {
        let exact = "a".repeat(64);
        assert!(validate_name("test", &exact).is_ok());
    }

    #[test]
    fn registry_without_default_rejected() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[dependencies]
regex = { package = "docs:regex", version = "^0.1.0" }
"#;
        let err = toml.parse::<crate::Manifest>().unwrap_err();
        assert!(
            matches!(err, ManifestError::NoDefaultRegistry { .. }),
            "expected NoDefaultRegistry, got {err:?}"
        );
    }

    #[test]
    fn registry_with_default_ok() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
default = "https://wa.dev"

[dependencies]
regex = { package = "docs:regex", version = "^0.1.0" }
"#;
        assert!(toml.parse::<crate::Manifest>().is_ok());
    }

    #[test]
    fn registry_with_explicit_alias_ok() {
        let toml = r#"
[package]
name = "app"
version = "0.1.0"

[registries]
custom = "https://registry.example.com"

[dependencies]
lib = { registry = "custom", package = "ns:lib", version = "^1.0.0" }
"#;
        assert!(toml.parse::<crate::Manifest>().is_ok());
    }

    #[test]
    fn invalid_package_name_rejected() {
        let toml = r#"
[package]
name = "my app"
version = "0.1.0"
"#;
        let err = toml.parse::<crate::Manifest>().unwrap_err();
        assert!(matches!(err, ManifestError::InvalidName { .. }));
    }

    #[test]
    fn invalid_package_version_rejected() {
        let toml = r#"
[package]
name = "app"
version = "not-a-version"
"#;
        let err = toml.parse::<crate::Manifest>().unwrap_err();
        assert!(matches!(err, ManifestError::InvalidVersion { .. }));
    }

    #[test]
    fn publishable_package_has_no_publish_errors() {
        let toml = r#"
[package]
namespace = "myorg"
name = "app"
version = "0.1.0"
description = "An app"
repository = "https://github.com/myorg/app"
license = "MIT"
authors = ["Alice"]
"#;
        let m = toml.parse::<crate::Manifest>().unwrap();
        assert!(super::validate_for_publish(&m).is_empty());
    }

    #[test]
    fn publish_collects_all_missing_requirements() {
        let toml = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        let m = toml.parse::<crate::Manifest>().unwrap();
        let errs = super::validate_for_publish(&m);
        use super::PublishError::*;
        assert!(errs.contains(&MissingNamespace), "{errs:?}");
        assert!(errs.contains(&MissingLicense), "{errs:?}");
        assert!(
            errs.contains(&MissingMetadata { field: "description" }),
            "{errs:?}"
        );
        assert!(
            errs.contains(&MissingMetadata { field: "repository" }),
            "{errs:?}"
        );
        assert!(
            errs.contains(&MissingMetadata { field: "authors" }),
            "{errs:?}"
        );
    }

    #[test]
    fn publish_disabled_is_an_error() {
        let toml = r#"
[package]
namespace = "myorg"
name = "app"
version = "0.1.0"
description = "An app"
repository = "https://github.com/myorg/app"
license = "MIT"
authors = ["Alice"]
publish = false
"#;
        let m = toml.parse::<crate::Manifest>().unwrap();
        assert!(super::validate_for_publish(&m).contains(&super::PublishError::PublishDisabled));
    }

    #[test]
    fn license_file_satisfies_publish_license_requirement() {
        let toml = r#"
[package]
namespace = "myorg"
name = "app"
version = "0.1.0"
description = "An app"
repository = "https://github.com/myorg/app"
license-file = "LICENSE-COMMERCIAL"
authors = ["Alice"]
"#;
        let m = toml.parse::<crate::Manifest>().unwrap();
        assert!(super::validate_for_publish(&m).is_empty());
    }

    #[test]
    fn publish_flags_workspace_dependency() {
        let toml = r#"
[package]
namespace = "myorg"
name = "app"
version = "0.1.0"
description = "An app"
repository = "https://github.com/myorg/app"
license = "MIT"
authors = ["Alice"]

[dependencies]
"ns:shared" = { workspace = true }
"#;
        let m = toml.parse::<crate::Manifest>().unwrap();
        let errs = super::validate_for_publish(&m);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                super::PublishError::WorkspaceDependency { dep_name } if dep_name == "ns:shared"
            )),
            "{errs:?}"
        );
    }

    #[test]
    fn publish_flags_path_dependency_without_source() {
        let toml = r#"
[package]
namespace = "myorg"
name = "app"
version = "0.1.0"
description = "An app"
repository = "https://github.com/myorg/app"
license = "MIT"
authors = ["Alice"]

[dependencies]
"lib:shared" = { path = "../shared" }
"#;
        let m = toml.parse::<crate::Manifest>().unwrap();
        let errs = super::validate_for_publish(&m);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                super::PublishError::PathDependencyWithoutSource { dep_name } if dep_name == "lib:shared"
            )),
            "{errs:?}"
        );
    }
}
