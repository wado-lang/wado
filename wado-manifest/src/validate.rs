use crate::manifest::{
    BuildSection, DependencySource, GeneratorInvocation, GeneratorModuleRef, Manifest,
    ManifestError,
};
use crate::version::{Version, VersionSpecifier};

/// Validate a parsed manifest for semantic consistency.
///
/// Called automatically by `Manifest::from_str`, but also available
/// separately for incremental validation (e.g., LSP).
pub fn validate(manifest: &Manifest) -> Result<(), ManifestError> {
    if let Some(pkg) = &manifest.package {
        validate_package(pkg)?;
    }
    validate_dependencies(manifest)?;
    if let Some(build) = &manifest.build {
        validate_build(manifest, build)?;
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
    Ok(())
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

fn validate_build(manifest: &Manifest, build: &BuildSection) -> Result<(), ManifestError> {
    let mut seen_from: Vec<(&str, &str)> = Vec::new();
    for (name, invocation) in &build.generators {
        validate_name("build.generators key", name)?;
        validate_generator_invocation(name, invocation, manifest)?;
        for (prior_name, prior_from) in &seen_from {
            if *prior_from == invocation.from.as_str() {
                return Err(ManifestError::DuplicateGeneratorFrom {
                    first: (*prior_name).to_string(),
                    second: name.clone(),
                    from: invocation.from.clone(),
                });
            }
        }
        seen_from.push((name.as_str(), invocation.from.as_str()));
    }
    Ok(())
}

fn validate_generator_invocation(
    name: &str,
    invocation: &GeneratorInvocation,
    manifest: &Manifest,
) -> Result<(), ManifestError> {
    if invocation.from.trim().is_empty() {
        return Err(ManifestError::InvalidGeneratorFrom {
            invocation: name.to_string(),
            reason: "must not be empty".to_string(),
        });
    }
    match &invocation.module {
        GeneratorModuleRef::Spec(spec) => validate_module_spec(name, spec, manifest)?,
        GeneratorModuleRef::Inline(dep) => {
            validate_dep_source(name, &dep.source, manifest, false)?;
        }
    }
    Ok(())
}

/// Validate a `module = "ns:name@ver[/submodule]"` spec string and check
/// that the `ns:name@ver` prefix matches an entry in `[build-dependencies]`.
fn validate_module_spec(
    invocation: &str,
    spec: &str,
    manifest: &Manifest,
) -> Result<(), ManifestError> {
    // Split off optional submodule path.
    let (ident, _submodule) = match spec.split_once('/') {
        Some((ident, rest)) => (ident, Some(rest)),
        None => (spec, None),
    };

    // `ident` must be `namespace:name@version`.
    let (ns_name, version) =
        ident
            .split_once('@')
            .ok_or_else(|| ManifestError::InvalidGeneratorModule {
                invocation: invocation.to_string(),
                value: spec.to_string(),
                reason: "expected `namespace:name@version`".to_string(),
            })?;
    let (namespace, name) =
        ns_name
            .split_once(':')
            .ok_or_else(|| ManifestError::InvalidGeneratorModule {
                invocation: invocation.to_string(),
                value: spec.to_string(),
                reason: "expected `namespace:name@version`".to_string(),
            })?;
    if namespace.is_empty() || name.is_empty() || version.is_empty() {
        return Err(ManifestError::InvalidGeneratorModule {
            invocation: invocation.to_string(),
            value: spec.to_string(),
            reason: "namespace, name, and version must be non-empty".to_string(),
        });
    }

    // Look for a matching [build-dependencies] entry. Match on registry-style
    // `namespace:name` package identity; git/path inline refs aren't resolved
    // by spec, only by the inline-module syntax.
    let found = manifest.build_dependencies.values().any(|dep| {
        matches!(
            &dep.source,
            DependencySource::Registry { package, .. } if package == ns_name
        )
    });
    if !found {
        return Err(ManifestError::UnknownGeneratorModule {
            invocation: invocation.to_string(),
            spec: spec.to_string(),
        });
    }

    Ok(())
}

fn validate_dep_key(name: &str) -> Result<(), ManifestError> {
    validate_name("dependency key", name)
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
command = "main.wado"

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
command = "main.wado"

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
command = "main.wado"

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
command = "main.wado"
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
command = "main.wado"
"#;
        let err = toml.parse::<crate::Manifest>().unwrap_err();
        assert!(matches!(err, ManifestError::InvalidVersion { .. }));
    }
}
