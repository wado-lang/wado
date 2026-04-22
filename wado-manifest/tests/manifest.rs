use wado_manifest::{DependencySource, GitPin, Manifest, ManifestError};

#[test]
fn full_manifest_from_wep() {
    let toml = r#"
[package]
namespace = "myorg"
name = "my-app"
version = "0.1.0"
command = "src/main.wado"
lib = "src/lib.wado"

[registries]
default = "https://wa.dev"

[dependencies]
router = { git = "https://github.com/user/router.git", version = "^1.0.0" }
regex = { package = "docs:regex", version = "^0.1.0" }
shared = { path = "../shared" }

[dev-dependencies]
bench = { git = "https://gitlab.com/user/bench.git", ref = "main" }
"#;
    let m: Manifest = toml.parse().unwrap();
    let pkg = m.package.as_ref().unwrap();
    assert_eq!(pkg.namespace.as_deref(), Some("myorg"));
    assert_eq!(pkg.name, "my-app");
    assert_eq!(pkg.version, "0.1.0");
    assert_eq!(pkg.command.as_deref(), Some("src/main.wado"));
    assert_eq!(pkg.lib.as_deref(), Some("src/lib.wado"));
    assert!(pkg.service.is_none());

    assert_eq!(m.registries["default"], "https://wa.dev");

    // Git dep with version
    assert!(matches!(
        &m.dependencies["router"].source,
        DependencySource::Git {
            url,
            pin: GitPin::Version(v),
        } if url == "https://github.com/user/router.git" && v == "^1.0.0"
    ));

    // Registry dep
    assert!(matches!(
        &m.dependencies["regex"].source,
        DependencySource::Registry {
            registry: None,
            package,
            version,
        } if package == "docs:regex" && version == "^0.1.0"
    ));

    // Path dep
    assert!(matches!(
        &m.dependencies["shared"].source,
        DependencySource::Path {
            path,
            publish_source: None,
        } if path == "../shared"
    ));

    // Dev dep with ref
    assert!(matches!(
        &m.dev_dependencies["bench"].source,
        DependencySource::Git {
            url,
            pin: GitPin::Ref(r),
        } if url == "https://gitlab.com/user/bench.git" && r == "main"
    ));
}

#[test]
fn workspace_manifest() {
    let toml = r#"
[workspace]
members = ["packages/*"]

[workspace.dependencies]
json = { package = "std:json", version = "^1.0.0" }

[workspace.dev-dependencies]
bench = { git = "https://gitlab.com/user/bench.git", version = "^0.1.0" }

[registries]
default = "https://wa.dev"
"#;
    let m: Manifest = toml.parse().unwrap();
    assert!(m.package.is_none());
    let ws = m.workspace.as_ref().unwrap();
    assert_eq!(ws.members, vec!["packages/*"]);
    assert!(ws.dependencies.contains_key("json"));
    assert!(ws.dev_dependencies.contains_key("bench"));
}

#[test]
fn workspace_root_with_package() {
    let toml = r#"
[workspace]
members = ["packages/*"]

[package]
name = "monorepo-root"
version = "0.1.0"
lib = "src/lib.wado"

[registries]
default = "https://wa.dev"
"#;
    let m: Manifest = toml.parse().unwrap();
    assert!(m.package.is_some());
    assert!(m.workspace.is_some());
}

#[test]
fn build_world_absent_default() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"
"#;
    let m: Manifest = toml.parse().unwrap();
    assert!(m.package.as_ref().unwrap().build_world.is_none());
}

#[test]
fn build_world_kiln_generator_accepted() {
    let toml = r#"
[package]
name = "gen"
version = "0.1.0"
build-world = "core:kiln/generator"
"#;
    let m: Manifest = toml.parse().unwrap();
    assert_eq!(
        m.package.as_ref().unwrap().build_world.as_deref(),
        Some("core:kiln/generator")
    );
}

#[test]
fn build_world_unknown_rejected() {
    let toml = r#"
[package]
name = "gen"
version = "0.1.0"
build-world = "wasi:cli/command"
"#;
    let err = toml.parse::<Manifest>().unwrap_err();
    assert!(matches!(
        err,
        ManifestError::InvalidBuildWorld { value } if value == "wasi:cli/command"
    ));
}

#[test]
fn missing_package_name() {
    let toml = r#"
[package]
version = "0.1.0"
"#;
    let err = toml.parse::<Manifest>().unwrap_err();
    assert!(matches!(
        err,
        ManifestError::MissingField {
            section,
            field,
        } if section == "package" && field == "name"
    ));
}

#[test]
fn missing_package_version() {
    let toml = r#"
[package]
name = "app"
"#;
    let err = toml.parse::<Manifest>().unwrap_err();
    assert!(matches!(
        err,
        ManifestError::MissingField {
            section,
            field,
        } if section == "package" && field == "version"
    ));
}

#[test]
fn no_source_specified() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

[dependencies]
oops = {}
"#;
    let err = toml.parse::<Manifest>().unwrap_err();
    assert!(matches!(err, ManifestError::ConflictingSource { .. }));
}

#[test]
fn git_and_package_conflict() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

[dependencies]
x = { git = "https://example.com/x.git", package = "ns:x", version = "^1.0.0" }
"#;
    let err = toml.parse::<Manifest>().unwrap_err();
    assert!(matches!(err, ManifestError::ConflictingSource { .. }));
}

#[test]
fn workspace_combined_with_path_rejected() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

[dependencies]
x = { workspace = true, path = "../x" }
"#;
    let err = toml.parse::<Manifest>().unwrap_err();
    assert!(matches!(err, ManifestError::ConflictingSource { .. }));
}

#[test]
fn git_neither_version_nor_ref() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

[dependencies]
x = { git = "https://example.com/x.git" }
"#;
    let err = toml.parse::<Manifest>().unwrap_err();
    assert!(matches!(err, ManifestError::GitVersionRefConflict { .. }));
}

#[test]
fn version_specifier_types() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

[registries]
default = "https://wa.dev"

[dependencies]
a = { package = "ns:a", version = "^1.0.0" }
b = { package = "ns:b", version = "~2.3.0" }
c = { package = "ns:c", version = "=0.5.1" }
"#;
    let m: Manifest = toml.parse().unwrap();
    assert!(matches!(
        &m.dependencies["a"].source,
        DependencySource::Registry { version, .. } if version == "^1.0.0"
    ));
    assert!(matches!(
        &m.dependencies["b"].source,
        DependencySource::Registry { version, .. } if version == "~2.3.0"
    ));
    assert!(matches!(
        &m.dependencies["c"].source,
        DependencySource::Registry { version, .. } if version == "=0.5.1"
    ));
}

#[test]
fn path_with_git_publish_source() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

[dependencies]
shared = { path = "../shared", git = "https://github.com/org/shared.git", version = "^0.1.0" }
"#;
    let m: Manifest = toml.parse().unwrap();
    match &m.dependencies["shared"].source {
        DependencySource::Path {
            path,
            publish_source,
        } => {
            assert_eq!(path, "../shared");
            assert!(matches!(
                publish_source.as_deref(),
                Some(DependencySource::Git { .. })
            ));
        }
        other => panic!("expected Path, got {other:?}"),
    }
}

#[test]
fn registry_with_named_registry() {
    let toml = r#"
[package]
name = "app"
version = "0.1.0"
command = "main.wado"

[registries]
custom = "https://registry.example.com"

[dependencies]
special = { registry = "custom", package = "ns:lib", version = "^1.0.0" }
"#;
    let m: Manifest = toml.parse().unwrap();
    assert!(matches!(
        &m.dependencies["special"].source,
        DependencySource::Registry {
            registry: Some(reg),
            package,
            version,
        } if reg == "custom" && package == "ns:lib" && version == "^1.0.0"
    ));
}

#[test]
fn manifest_without_package_section() {
    // Pure workspace root
    let toml = r#"
[workspace]
members = ["packages/*"]
"#;
    let m: Manifest = toml.parse().unwrap();
    assert!(m.package.is_none());
    assert!(m.workspace.is_some());
}

#[test]
fn multiple_entry_points() {
    let toml = r#"
[package]
name = "devtool"
version = "0.1.0"
command = "src/cli.wado"
service = "src/dashboard.wado"
lib = "src/lib.wado"
"#;
    let m: Manifest = toml.parse().unwrap();
    let pkg = m.package.as_ref().unwrap();
    assert_eq!(pkg.command.as_deref(), Some("src/cli.wado"));
    assert_eq!(pkg.service.as_deref(), Some("src/dashboard.wado"));
    assert_eq!(pkg.lib.as_deref(), Some("src/lib.wado"));
}

#[test]
fn error_display() {
    let err = ManifestError::BareVersion {
        dep_name: "regex".to_string(),
        version: "1.0.0".to_string(),
    };
    let msg = err.to_string();
    assert!(msg.contains("regex"));
    assert!(msg.contains("1.0.0"));
    assert!(msg.contains("prefix"));
}
