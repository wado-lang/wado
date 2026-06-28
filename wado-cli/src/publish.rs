//! `wado publish` — publish the package to a registry.
//!
//! Only `--dry-run` is implemented: it runs the publish-readiness checks
//! ([`wado_manifest::validate_for_publish`]) and reports any problems. The
//! actual OCI upload (via `wkg`, with metadata embedded into the component) is
//! not wired yet, so a non-dry-run invocation errors instead of pretending to
//! publish.
//!
//! In a workspace, publishing is gated to the workspace root: it publishes every
//! publishable member together at the shared (force-inherited) version, so the
//! registry can never end up with members at mismatched versions. Running it
//! from a member directory is an error pointing at the root.

use std::fmt::Write as _;

use wado_manifest::{Manifest, PublishError, validate_for_publish};

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
    writeln!(buf, "Check whether the package can be published.").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    writeln!(
        buf,
        "      --dry-run  Run publish-readiness checks without uploading"
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

pub fn run(opts: PublishOptions) -> Result<(), CliExit> {
    if !opts.dry_run {
        return Err(CliExit::error(
            "wado publish: only --dry-run is supported for now \
             (OCI upload via wkg is not yet implemented)",
        ));
    }

    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    let project = discover(&cwd)
        .map_err(CliExit::error)?
        .ok_or_else(|| CliExit::error("no wado.toml found"))?;

    if project.manifest.workspace.is_some() {
        return publish_workspace_dry_run(&project);
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
    publish_single_dry_run(&project.manifest)
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

fn publish_single_dry_run(manifest: &Manifest) -> Result<(), CliExit> {
    match classify(manifest) {
        Verdict::Ready(coord) => {
            eprintln!("{coord} is ready to publish");
            Ok(())
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

fn publish_workspace_dry_run(root: &ProjectManifest) -> Result<(), CliExit> {
    emit_manifest_warnings(root);
    let members = root
        .manifest
        .workspace
        .as_ref()
        .map(|w| w.members.as_slice())
        .unwrap_or_default();
    let member_dirs = crate::manifest::workspace_member_dirs(&root.root, members);

    let mut ready: Vec<String> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut failed: Vec<(String, Vec<PublishError>)> = Vec::new();

    let mut candidates: Vec<(String, Manifest)> = vec![(".".to_string(), root.manifest.clone())];
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
        candidates.push((label, project.manifest));
    }

    for (label, manifest) in &candidates {
        match classify(manifest) {
            Verdict::NotAPackage => {}
            Verdict::Skipped(reason) => skipped.push((label.clone(), reason)),
            Verdict::Ready(coord) => ready.push(coord),
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
    eprintln!(
        "{} package(s) ready to publish: {}",
        ready.len(),
        ready.join(", ")
    );
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
}
