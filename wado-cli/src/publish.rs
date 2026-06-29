//! `wado publish` — publish the package to a registry.
//!
//! A facade over the build path and `wkg`: it runs the publish-readiness checks
//! ([`wado_manifest::validate_for_publish`]), builds each publishable world's
//! component with `[package]` metadata embedded, then shells out to `wkg oci
//! push` to upload each as an OCI artifact. `--dry-run` stops after the checks
//! and reports any problems without building or uploading.
//!
//! Each world is a distinct artifact: the library world publishes to the bare
//! repository `<prefix>/<ns>/<name>`, and every other `[world]` entry (unless
//! `publish = false`) to a `/<world>` sub-path. The push target is the `default`
//! registry; a missing or non-`oci://` default is an error. Authentication is
//! delegated to `wkg` and the ambient OCI credential store (`docker login`) or
//! its `WKG_OCI_USERNAME` / `WKG_OCI_PASSWORD` override; Wado stores no
//! credentials.
//!
//! In a workspace, publishing is gated to the workspace root: it publishes every
//! publishable member together at the shared (force-inherited) version against
//! the root's `[registries]`, so the registry can never end up with members at
//! mismatched versions. Running it from a member directory is an error pointing
//! at the root.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use wado_manifest::{Manifest, Package, PublishError, validate_for_publish};

use crate::args::{self, CliExit};
use crate::manifest::{ProjectManifest, discover, emit_manifest_warnings};

#[derive(Debug)]
pub struct PublishOptions {
    dry_run: bool,
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado publish [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Build the package and publish it to a registry via wkg."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    writeln!(
        buf,
        "      --dry-run  Run publish-readiness checks without building or uploading"
    )
    .unwrap();
    writeln!(buf, "  -h, --help     Show this help message").unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<PublishOptions, CliExit> {
    let usage = format_usage();
    let mut dry_run = false;
    while let Some(arg) = args::next_arg(&mut parser)? {
        match arg {
            lexopt::Arg::Long("help") | lexopt::Arg::Short('h') => {
                return Err(CliExit::help(usage));
            }
            lexopt::Arg::Long("dry-run") => dry_run = true,
            other => return Err(args::unexpected_arg(other, &usage)),
        }
    }
    Ok(PublishOptions { dry_run })
}

pub async fn run(opts: PublishOptions) -> Result<(), CliExit> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    let project = discover(&cwd)
        .map_err(CliExit::error)?
        .ok_or_else(|| CliExit::error("no wado.toml found"))?;

    if project.manifest.workspace.is_some() {
        return publish_workspace(&project, opts.dry_run).await;
    }
    if let Some(root) =
        crate::manifest::governing_workspace_root_dir(&project.root).map_err(CliExit::error)?
    {
        return Err(CliExit::error(format!(
            "this package belongs to a workspace; run `wado publish` from the workspace \
             root to publish all members together at the shared version:\n  {}",
            root.display()
        )));
    }

    emit_manifest_warnings(&project);
    publish_single(&project, opts.dry_run).await
}

/// One package's publish-readiness verdict.
enum Verdict {
    /// No `[package]` — not a publishable unit; ignored.
    NotAPackage,
    /// Intentionally not published (`publish = false` or no `namespace`).
    Skipped(String),
    /// Ready: the `namespace:name@version` coordinate.
    Ready(String),
    /// Has unmet publish requirements.
    Failed(Vec<PublishError>),
}

fn classify(manifest: &Manifest) -> Verdict {
    let Some(pkg) = manifest.package.as_ref() else {
        return Verdict::NotAPackage;
    };
    if !pkg.publish {
        return Verdict::Skipped("publish = false".to_string());
    }
    let Some(namespace) = pkg.namespace.as_deref() else {
        return Verdict::Skipped("no namespace".to_string());
    };
    let problems = validate_for_publish(manifest);
    if problems.is_empty() {
        Verdict::Ready(format!("{namespace}:{}@{}", pkg.name, pkg.version))
    } else {
        Verdict::Failed(problems)
    }
}

async fn publish_single(project: &ProjectManifest, dry_run: bool) -> Result<(), CliExit> {
    match classify(&project.manifest) {
        Verdict::Ready(coord) => {
            if dry_run {
                eprintln!("{coord} is ready to publish");
                Ok(())
            } else {
                publish_package(project, &coord, &project.manifest.registries).await
            }
        }
        Verdict::Failed(problems) => Err(problems_error(
            "package is not ready to publish:",
            &problems,
        )),
        Verdict::NotAPackage => Err(CliExit::error("no [package] to publish")),
        Verdict::Skipped(reason) => Err(CliExit::error(format!(
            "package is not publishable: {reason}"
        ))),
    }
}

async fn publish_workspace(root: &ProjectManifest, dry_run: bool) -> Result<(), CliExit> {
    emit_manifest_warnings(root);
    let members = root
        .manifest
        .workspace
        .as_ref()
        .map(|w| w.members.as_slice())
        .unwrap_or_default();
    let member_dirs = crate::manifest::workspace_member_dirs(&root.root, members);

    // Keep each candidate's `ProjectManifest` so a non-dry-run publish can build
    // it; the root's own `[package]` (if any) publishes alongside the members.
    let mut candidates: Vec<(String, ProjectManifest)> = vec![(
        ".".to_string(),
        ProjectManifest {
            manifest: root.manifest.clone(),
            root: root.root.clone(),
        },
    )];
    for dir in member_dirs {
        let project = discover(&dir)
            .map_err(CliExit::error)?
            .ok_or_else(|| CliExit::error(format!("no wado.toml in member {}", dir.display())))?;
        emit_manifest_warnings(&project);
        let label = dir
            .strip_prefix(&root.root)
            .unwrap_or(&dir)
            .display()
            .to_string();
        candidates.push((label, project));
    }

    let mut ready: Vec<(String, &ProjectManifest)> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut failed: Vec<(String, Vec<PublishError>)> = Vec::new();
    for (label, project) in &candidates {
        match classify(&project.manifest) {
            Verdict::NotAPackage => {}
            Verdict::Skipped(reason) => skipped.push((label.clone(), reason)),
            Verdict::Ready(coord) => ready.push((coord, project)),
            Verdict::Failed(problems) => failed.push((label.clone(), problems)),
        }
    }

    if !failed.is_empty() {
        let mut msg = String::from("workspace is not ready to publish:");
        for (label, problems) in &failed {
            let _ = write!(msg, "\n  {label}:");
            for problem in problems {
                let _ = write!(msg, "\n    - {problem}");
            }
        }
        return Err(CliExit::error(msg));
    }
    for (label, reason) in &skipped {
        eprintln!("(skip) {label}: {reason}");
    }
    if ready.is_empty() {
        return Err(CliExit::error("no publishable packages in this workspace"));
    }

    if dry_run {
        let coords: Vec<&str> = ready.iter().map(|(c, _)| c.as_str()).collect();
        eprintln!(
            "{} package(s) ready to publish: {}",
            coords.len(),
            coords.join(", ")
        );
        return Ok(());
    }

    // Publish is a root-only operation, so every member resolves against the
    // workspace root's `[registries]`, not its own.
    for (coord, project) in &ready {
        publish_package(project, coord, &root.manifest.registries).await?;
    }
    Ok(())
}

/// Which world a publish target builds. The library world lives at the bare
/// repository; a hosted world gets the `/<segment>` sub-path. Modeling it as an
/// enum keeps "exactly one world kind" structural instead of a pair of
/// `Option`s with an implicit invariant.
enum BuildWorld {
    /// Library world; the value is its FQ name (passed as `--lib`).
    Lib(String),
    /// Hosted `[world]` entry; the value is its FQ name (passed as `--world`).
    Hosted(String),
}

/// One world to publish: where its component is built and the OCI sub-path it
/// targets (`None` for the library world, which lives at the bare repository).
struct PublishTarget {
    subpath: Option<String>,
    entry: std::path::PathBuf,
    output: std::path::PathBuf,
    world: BuildWorld,
}

/// Every world a package publishes: the library world (bare repository) plus
/// each `[world]` entry not opted out with `publish = false`.
fn publishable_worlds(project: &ProjectManifest) -> Result<Vec<PublishTarget>, CliExit> {
    let pkg = project
        .manifest
        .package
        .as_ref()
        .ok_or_else(|| CliExit::error("no [package] to publish"))?;
    let root = &project.root;
    let mut targets = Vec::new();
    if let Some(lib_rel) = pkg.lib.as_deref() {
        targets.push(PublishTarget {
            subpath: None,
            entry: root.join(lib_rel),
            output: crate::compile::build_output_path(root, "lib"),
            world: BuildWorld::Lib(crate::manifest::lib_world_fq(pkg)?),
        });
    }
    for (world_fq, entry) in &project.manifest.world {
        if !entry.publish {
            continue;
        }
        let segment = crate::compile::world_path_segment(world_fq);
        targets.push(PublishTarget {
            entry: root.join(&entry.entry),
            output: crate::compile::build_output_path(root, &segment),
            subpath: Some(segment),
            world: BuildWorld::Hosted(world_fq.clone()),
        });
    }
    Ok(targets)
}

/// Build and push every publishable world of one ready package: build each
/// world's component, resolve its OCI reference, then `wkg oci push` it.
/// `registries` is the workspace root's (or the package's own, when standalone)
/// `[registries]`, since publish is a root-only operation.
async fn publish_package(
    project: &ProjectManifest,
    coord: &str,
    registries: &indexmap::IndexMap<String, String>,
) -> Result<(), CliExit> {
    let pkg = project
        .manifest
        .package
        .as_ref()
        .ok_or_else(|| CliExit::error("no [package] to publish"))?;
    let targets = publishable_worlds(project)?;
    if targets.is_empty() {
        return Err(CliExit::error(format!(
            "{coord} declares no publishable world; add `[package].lib` or a `[world]` entry"
        )));
    }
    if crate::metadata_embed::working_tree_dirty(&project.root) == Some(true) {
        eprintln!(
            "warning: working tree has uncommitted changes; publishing {coord} \
             without a `revision` annotation"
        );
    }
    for target in &targets {
        let reference = resolve_push_target(registries, pkg, target.subpath.as_deref())?;
        let (lib_world, target_world) = match &target.world {
            BuildWorld::Lib(fq) => (Some(fq.clone()), None),
            BuildWorld::Hosted(fq) => (None, Some(fq.clone())),
        };
        crate::compile::build_publish_world(&target.entry, &target.output, lib_world, target_world)
            .await?;
        eprintln!(
            "Publishing {coord} ({}) -> {reference}",
            target.subpath.as_deref().unwrap_or("lib")
        );
        wkg_oci_push(&reference, &target.output)?;
    }
    eprintln!("Published {coord} ({} artifact(s))", targets.len());
    Ok(())
}

/// The OCI reference `wkg oci push` targets:
/// `<host>/<prefix>/<namespace>/<name>[/<world>]:<version>`, derived from the
/// `default` registry (the only supported publish destination) and the package
/// coordinate. `registries` comes from the workspace root when publishing a
/// workspace, since `wado publish` is a root-only operation. `subpath` is the
/// world segment for a non-library world. Errors when no default registry is
/// set or it is not an `oci://` URL.
fn resolve_push_target(
    registries: &indexmap::IndexMap<String, String>,
    pkg: &Package,
    subpath: Option<&str>,
) -> Result<String, CliExit> {
    let registry = registries.get("default").ok_or_else(|| {
        CliExit::error(
            "publishing requires a default registry; set it in wado.toml:\n  \
             [registries]\n  default = \"oci://ghcr.io/yourorg\"",
        )
    })?;
    let base = registry.strip_prefix("oci://").ok_or_else(|| {
        CliExit::error(format!(
            "default registry {registry:?} is not an oci:// URL; \
             only OCI registries are supported for publish"
        ))
    })?;
    let base = base.trim_end_matches('/');
    let namespace = pkg
        .namespace
        .as_deref()
        .ok_or_else(|| CliExit::error("[package].namespace is required to publish"))?;
    // OCI repository components must be lowercase; Wado names allow uppercase, so
    // catch a non-pushable coordinate here with a clear message rather than a
    // cryptic `wkg` failure.
    check_oci_path_component("[package].namespace", namespace)?;
    check_oci_path_component("[package].name", &pkg.name)?;
    if let Some(segment) = subpath {
        check_oci_path_component("world", segment)?;
    }
    check_oci_tag(&pkg.version)?;
    let mut repo = format!("{base}/{namespace}/{name}", name = pkg.name);
    if let Some(segment) = subpath {
        repo.push('/');
        repo.push_str(segment);
    }
    Ok(format!("{repo}:{version}", version = pkg.version))
}

/// Reject an OCI repository path component that isn't pushable: components must
/// be lowercase `[a-z0-9]` with single `.`/`_`/`-` separators (no leading or
/// trailing separator).
fn check_oci_path_component(field: &str, value: &str) -> Result<(), CliExit> {
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let valid = !value.is_empty()
        && value
            .bytes()
            .all(|b| alnum(b) || matches!(b, b'.' | b'_' | b'-'))
        && value.bytes().next().is_some_and(alnum)
        && value.bytes().next_back().is_some_and(alnum);
    if valid {
        Ok(())
    } else {
        Err(CliExit::error(format!(
            "{field} {value:?} cannot be published to an OCI registry: \
             repository names must be lowercase letters, digits, and `.`/`_`/`-`"
        )))
    }
}

/// Reject a version that isn't a valid OCI image tag — notably a semver build
/// metadata `+`, which OCI tags disallow.
fn check_oci_tag(version: &str) -> Result<(), CliExit> {
    let valid = (1..=128).contains(&version.len())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && version
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(CliExit::error(format!(
            "version {version:?} cannot be an OCI image tag: \
             tags allow letters, digits, and `_`/`.`/`-` (no `+` build metadata)"
        )))
    }
}

const WKG_INSTALL_HINT: &str = "install wasm-pkg-tools: `cargo install wkg` (https://github.com/bytecodealliance/wasm-pkg-tools)";

fn wkg_oci_push(reference: &str, component: &Path) -> Result<(), CliExit> {
    run_wkg_push("wkg", reference, component)
}

/// Run `<program> oci push <reference> <component>`, mapping a missing binary to
/// install guidance. `program` is injectable so the missing-binary path is
/// testable without depending on `wkg` being absent.
fn run_wkg_push(program: &str, reference: &str, component: &Path) -> Result<(), CliExit> {
    let status = Command::new(program)
        .args(["oci", "push", reference])
        .arg(component)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliExit::error(format!("`{program}` not found on PATH; {WKG_INSTALL_HINT}"))
            } else {
                CliExit::error(format!("failed to run `{program}`: {e}"))
            }
        })?;
    if !status.success() {
        return Err(CliExit::error(format!(
            "`{program} oci push` failed for {reference}"
        )));
    }
    Ok(())
}

fn problems_error(header: &str, problems: &[PublishError]) -> CliExit {
    let mut msg = String::from(header);
    for problem in problems {
        let _ = write!(msg, "\n  - {problem}");
    }
    CliExit::error(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_defaults_to_no_dry_run() {
        let parser = lexopt::Parser::from_args(Vec::<String>::new());
        let opts = parse_args(parser).unwrap();
        assert!(!opts.dry_run);
    }

    #[test]
    fn parse_args_accepts_dry_run() {
        let parser = lexopt::Parser::from_args(vec!["--dry-run"]);
        let opts = parse_args(parser).unwrap();
        assert!(opts.dry_run);
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let parser = lexopt::Parser::from_args(vec!["--nope"]);
        let err = parse_args(parser).unwrap_err();
        assert_eq!(err.exit_code, 1);
    }

    fn manifest(toml: &str) -> Manifest {
        toml.parse().unwrap()
    }

    #[test]
    fn classify_ready_package() {
        let m = manifest(
            "[package]\nnamespace = \"org\"\nname = \"app\"\nversion = \"0.1.0\"\ndescription = \"d\"\nrepository = \"https://x/y\"\nlicense = \"MIT\"\nauthors = [\"A\"]\n",
        );
        assert!(matches!(classify(&m), Verdict::Ready(c) if c == "org:app@0.1.0"));
    }

    #[test]
    fn classify_skips_publish_false_and_no_namespace() {
        let no_ns = manifest("[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
        assert!(matches!(classify(&no_ns), Verdict::Skipped(r) if r == "no namespace"));
        let opted_out = manifest(
            "[package]\nnamespace = \"org\"\nname = \"app\"\nversion = \"0.1.0\"\npublish = false\n",
        );
        assert!(matches!(classify(&opted_out), Verdict::Skipped(r) if r == "publish = false"));
    }

    #[test]
    fn classify_failed_when_metadata_missing() {
        let m = manifest("[package]\nnamespace = \"org\"\nname = \"app\"\nversion = \"0.1.0\"\n");
        assert!(matches!(classify(&m), Verdict::Failed(p) if !p.is_empty()));
    }

    #[test]
    fn classify_not_a_package_for_workspace_only_manifest() {
        let m = manifest("[workspace]\nmembers = [\"packages/*\"]\n");
        assert!(matches!(classify(&m), Verdict::NotAPackage));
    }

    fn ready_manifest_with_registry(registry: &str) -> Manifest {
        manifest(&format!(
            "[package]\nnamespace = \"org\"\nname = \"app\"\nversion = \"0.1.0\"\n\
             [registries]\ndefault = \"{registry}\"\n"
        ))
    }

    #[test]
    fn resolve_push_target_library_uses_bare_repository() {
        let m = ready_manifest_with_registry("oci://ghcr.io/acme");
        let target = resolve_push_target(&m.registries, m.package.as_ref().unwrap(), None).unwrap();
        assert_eq!(target, "ghcr.io/acme/org/app:0.1.0");
    }

    #[test]
    fn resolve_push_target_world_appends_subpath() {
        let m = ready_manifest_with_registry("oci://ghcr.io/acme");
        let target = resolve_push_target(
            &m.registries,
            m.package.as_ref().unwrap(),
            Some("core-kiln-generator"),
        )
        .unwrap();
        assert_eq!(target, "ghcr.io/acme/org/app/core-kiln-generator:0.1.0");
    }

    #[test]
    fn resolve_push_target_trims_trailing_slash() {
        let m = ready_manifest_with_registry("oci://ghcr.io/acme/");
        let target = resolve_push_target(&m.registries, m.package.as_ref().unwrap(), None).unwrap();
        assert_eq!(target, "ghcr.io/acme/org/app:0.1.0");
    }

    #[test]
    fn resolve_push_target_errors_without_default_registry() {
        let m = manifest("[package]\nnamespace = \"org\"\nname = \"app\"\nversion = \"0.1.0\"\n");
        let err =
            resolve_push_target(&m.registries, m.package.as_ref().unwrap(), None).unwrap_err();
        assert!(
            err.message.contains("default registry"),
            "{:?}",
            err.message
        );
    }

    #[test]
    fn resolve_push_target_rejects_non_oci_registry() {
        let m = ready_manifest_with_registry("https://wa.dev");
        let err =
            resolve_push_target(&m.registries, m.package.as_ref().unwrap(), None).unwrap_err();
        assert!(err.message.contains("oci://"), "{:?}", err.message);
    }

    #[test]
    fn resolve_push_target_rejects_uppercase_coordinate() {
        let m = manifest(
            "[package]\nnamespace = \"MyOrg\"\nname = \"app\"\nversion = \"0.1.0\"\n\
             [registries]\ndefault = \"oci://ghcr.io\"\n",
        );
        let err =
            resolve_push_target(&m.registries, m.package.as_ref().unwrap(), None).unwrap_err();
        assert!(err.message.contains("lowercase"), "{:?}", err.message);
    }

    #[test]
    fn resolve_push_target_rejects_uppercase_world_segment() {
        let m = ready_manifest_with_registry("oci://ghcr.io/acme");
        let err = resolve_push_target(&m.registries, m.package.as_ref().unwrap(), Some("Foo-Bar"))
            .unwrap_err();
        assert!(err.message.contains("lowercase"), "{:?}", err.message);
    }

    #[test]
    fn resolve_push_target_rejects_build_metadata_version() {
        let m = manifest(
            "[package]\nnamespace = \"org\"\nname = \"app\"\nversion = \"1.0.0+build.5\"\n\
             [registries]\ndefault = \"oci://ghcr.io\"\n",
        );
        let err =
            resolve_push_target(&m.registries, m.package.as_ref().unwrap(), None).unwrap_err();
        assert!(err.message.contains("tag"), "{:?}", err.message);
    }

    #[test]
    fn run_wkg_push_missing_binary_reports_install_hint() {
        let err = run_wkg_push(
            "wkg-definitely-not-on-path-xyz",
            "ghcr.io/acme/org/app:0.1.0",
            Path::new("/tmp/does-not-matter.wasm"),
        )
        .unwrap_err();
        assert!(
            err.message.contains("cargo install wkg"),
            "{:?}",
            err.message
        );
    }
}
