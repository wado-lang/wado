//! `wado check` — CI-side integrity check for committed-source Kiln
//! workflows. Re-runs every Kiln invocation, byte-compares each output
//! against the on-disk file, and treats Kiln warnings as errors by
//! default.
//!
//! See [WEP: Kiln](../../docs/wep-2026-04-12-kiln.md), section "The
//! `wado check` command".

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use wado_compiler::{Code, LogLevel};

use crate::args::{self, CliExit};
use crate::compiler_host::FilesystemCompilerHost;
use crate::kiln_driver::{CheckOutcome, PipelineError};
use crate::kiln_provider::CliGeneratorProvider;

#[derive(Debug)]
pub struct CheckOptions {
    pub input: String,
    /// `false` (default) → Kiln warnings produce a non-zero exit. `true`
    /// → keep them as warnings (developer-friendly local triage).
    pub warn_only: bool,
    pub log_level: LogLevel,
}

#[derive(Clone, Copy)]
enum Opt {
    Warn,
    LogLevel,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Warn, Self::LogLevel, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Warn => args::OptSpec {
                long: Some("warn"),
                short: None,
                value: None,
                desc: "Keep Kiln warnings as warnings instead of promoting them to errors",
            },
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado check [options] <file.wado>").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Verify a Wado source file (and its Kiln generators) without emitting Wasm.",
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Re-runs every Kiln generator and compares the output against the on-disk\n\
         source. By default, any Kiln divergence (modified, regenerated, or stale\n\
         output) exits non-zero — suitable for CI gates on committed-source workflows.",
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse command-line arguments for the `check` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<CheckOptions, CliExit> {
    let usage = format_usage();
    let mut input: Option<String> = None;
    let mut warn_only = false;
    let mut log_level = LogLevel::default();
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Warn => warn_only = true,
                Opt::LogLevel => log_level = args::parse_log_level_arg(&mut parser)?,
                Opt::Help => {
                    return Err(CliExit::help(usage));
                }
            }
        } else if let Value(val) = arg {
            args::reject_multiple_inputs(&input)?;
            input = Some(val.to_string_lossy().to_string());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }
    let input = args::require_input(input, &usage)?;
    Ok(CheckOptions {
        input,
        warn_only,
        log_level,
    })
}

pub async fn run(opts: CheckOptions) {
    let path = Path::new(&opts.input);
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::with_log_level(base_path, opts.log_level);

    let manifest_pair = crate::compile::load_nearest_manifest(path);
    let manifest_root = manifest_pair
        .as_ref()
        .map(|(_, root)| root.clone())
        .unwrap_or_else(|| {
            path.parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::path::PathBuf::from("."))
        });
    let inline = crate::compile::collect_inline_invocations_for_entry(path, &manifest_root);

    let outcome = if inline.is_empty() {
        CheckOutcome::default()
    } else {
        let manifest = manifest_pair
            .map(|(m, _)| m)
            .unwrap_or_else(crate::compile::empty_manifest);
        let provider = CliGeneratorProvider::new(manifest_root.clone());
        match crate::kiln_driver::check_pipeline(
            &manifest,
            &manifest_root,
            &host,
            &provider,
            inline,
        )
        .await
        {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{}", FormatPipelineError(&e));
                process::exit(1);
            }
        }
    };

    let kiln_drift = !outcome.stale.is_empty() || !outcome.missing.is_empty();

    // Run the rest of the compile pipeline so type-/resolve-level errors
    // also gate `wado check`. We discard the produced wasm; codegen-skip
    // is a follow-up optimization.
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wado check: failed to read {}: {e}", path.display());
            process::exit(1);
        }
    };
    let compiler_options = wado_compiler::CompilerOptions {
        log_level: Some(opts.log_level),
        invocations: outcome.invocations.clone(),
        ..Default::default()
    };
    let compile_result = wado_compiler::compile_with_options(
        &source,
        &host,
        Some(&opts.input),
        compiler_options,
    )
    .await;

    let has_compile_errors = host.has_errors() || compile_result.is_err();
    let has_kiln_warnings = host
        .diagnostics()
        .into_iter()
        .any(|d| is_kiln_diagnostic(&d.code));

    if has_compile_errors {
        process::exit(1);
    }
    if !opts.warn_only && (kiln_drift || has_kiln_warnings) {
        eprintln!(
            "wado check: Kiln integrity check failed — \
             one or more generators produced output that differs from on-disk source. \
             Pass --warn to keep warnings as warnings."
        );
        process::exit(1);
    }
}

fn is_kiln_diagnostic(code: &Code) -> bool {
    matches!(
        code,
        Code::KilnStaleCache
            | Code::KilnGeneratorForbiddenImport
            | Code::KilnMissingWith
            | Code::KilnGeneratedModified
            | Code::KilnGeneratedRegenerated
            | Code::KilnGeneratedStaleOnDisk
    )
}

struct FormatPipelineError<'a>(&'a PipelineError);

impl std::fmt::Display for FormatPipelineError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wado check: {}", self.0)
    }
}
