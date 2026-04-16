use std::fmt::Write as _;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};
use crate::query_adapter;

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
        QueryKind::Diagnostics => query_adapter::run_diagnostics(&opts.input, opts.json).await,
    }
}
