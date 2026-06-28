//! `wado publish` — publish the package to a registry.
//!
//! Only `--dry-run` is implemented: it runs the publish-readiness checks
//! ([`wado_manifest::validate_for_publish`]) and reports any problems. The
//! actual OCI upload (via `wkg`, with metadata embedded into the component) is
//! not wired yet, so a non-dry-run invocation errors instead of pretending to
//! publish.

use std::fmt::Write as _;

use wado_manifest::validate_for_publish;

use crate::args::{self, CliExit};
use crate::manifest::discover;

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
            lexopt::Arg::Long("help") | lexopt::Arg::Short('h') => return Err(CliExit::help(usage)),
            lexopt::Arg::Long("dry-run") => dry_run = true,
            other => return Err(args::unexpected_arg(other, &usage)),
        }
    }
    Ok(PublishOptions { dry_run })
}

pub fn run(opts: PublishOptions) -> Result<(), CliExit> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    let project = discover(&cwd)
        .map_err(CliExit::error)?
        .ok_or_else(|| CliExit::error("no wado.toml found"))?;
    crate::manifest::emit_manifest_warnings(&project);

    if !opts.dry_run {
        return Err(CliExit::error(
            "wado publish: only --dry-run is supported for now \
             (OCI upload via wkg is not yet implemented)",
        ));
    }

    let problems = validate_for_publish(&project.manifest);
    if problems.is_empty() {
        // `validate_for_publish` reports `NoPackage`/`MissingNamespace` as
        // problems, so an empty result guarantees a namespaced package here.
        let pkg = project
            .manifest
            .package
            .as_ref()
            .expect("publishable manifest has a [package]");
        let namespace = pkg
            .namespace
            .as_deref()
            .expect("publishable package has a namespace");
        eprintln!(
            "{}:{}@{} is ready to publish",
            namespace, pkg.name, pkg.version
        );
        Ok(())
    } else {
        let mut msg = String::from("package is not ready to publish:");
        for problem in &problems {
            write!(msg, "\n  - {problem}").unwrap();
        }
        Err(CliExit::error(msg))
    }
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
}
