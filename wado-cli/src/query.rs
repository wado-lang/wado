use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use serde_json::json;

use crate::args::{self, CliExit};
use crate::compiler_host::FilesystemCompilerHost;

#[derive(Clone, Copy)]
enum QueryKind {
    Diagnostics,
}

pub struct QueryOptions {
    kind: QueryKind,
    input: String,
    json: bool,
}

#[derive(Clone, Copy)]
enum Opt {
    Json,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Json, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Json => args::OptSpec {
                long: Some("json"),
                short: None,
                value: None,
                desc: "Output as JSON",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado query <kind> [options] <file.wado>").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Query compiler information about a source file.").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Kinds:").unwrap();
    writeln!(buf, "  diagnostics    Show errors and warnings").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse command-line arguments for the `query` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<QueryOptions, CliExit> {
    let usage = format_usage();
    let mut kind: Option<QueryKind> = None;
    let mut input: Option<String> = None;
    let mut json = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Json => json = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            let val_str = val.to_string_lossy();
            if kind.is_none() {
                kind = Some(match val_str.as_ref() {
                    "diagnostics" => QueryKind::Diagnostics,
                    other => {
                        return Err(CliExit::error(format!(
                            "unknown query kind '{other}'. Available: diagnostics"
                        )));
                    }
                });
            } else {
                args::reject_multiple_inputs(&input)?;
                input = Some(val_str.into_owned());
            }
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let kind = kind.ok_or_else(|| CliExit::error_with_usage("missing query kind", &usage))?;
    let input = input.ok_or_else(|| CliExit::error_with_usage("missing input file", &usage))?;

    Ok(QueryOptions { kind, input, json })
}

pub async fn run(opts: QueryOptions) {
    match opts.kind {
        QueryKind::Diagnostics => run_diagnostics(&opts.input, opts.json).await,
    }
}

async fn run_diagnostics(filename: &str, json_output: bool) {
    let path = Path::new(filename);
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {e}", path.display());
            process::exit(1);
        }
    };

    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::silent(base_path);

    let uri = format!("file://{}", path.canonicalize().unwrap_or_else(|_| path.to_path_buf()).display());
    let mut engine = wado_lsp::Engine::new();
    engine.open_document(&uri, source);

    let diagnostics = engine.diagnostics(&uri, &host).await;

    if json_output {
        let json_diags: Vec<serde_json::Value> = diagnostics
            .iter()
            .map(|d| {
                json!({
                    "file": filename,
                    "range": {
                        "start": { "line": d.range.start.line, "character": d.range.start.character },
                        "end": { "line": d.range.end.line, "character": d.range.end.character },
                    },
                    "severity": match d.severity {
                        wado_lsp::Severity::Error => "error",
                        wado_lsp::Severity::Warning => "warning",
                        wado_lsp::Severity::Information => "information",
                        wado_lsp::Severity::Hint => "hint",
                    },
                    "code": d.code,
                    "message": d.message,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_diags).unwrap());
    } else {
        if diagnostics.is_empty() {
            println!("No diagnostics.");
            return;
        }
        for d in &diagnostics {
            let severity = match d.severity {
                wado_lsp::Severity::Error => "error",
                wado_lsp::Severity::Warning => "warning",
                wado_lsp::Severity::Information => "info",
                wado_lsp::Severity::Hint => "hint",
            };
            // Display as 1-based line/column for human readability
            println!(
                "{}:{}:{}: {}: {} [{}]",
                filename,
                d.range.start.line + 1,
                d.range.start.character + 1,
                severity,
                d.message,
                d.code,
            );
        }
    }

    if diagnostics.iter().any(|d| matches!(d.severity, wado_lsp::Severity::Error)) {
        process::exit(1);
    }
}
