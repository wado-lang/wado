use std::fmt::Write as _;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};
use crate::query_adapter;

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryKind {
    Diagnostics,
    References,
    DocumentHighlight,
    Definition,
}

pub struct QueryOptions {
    kind: QueryKind,
    input: String,
    json: bool,
    line: Option<u32>,
    column: Option<u32>,
    include_declaration: bool,
}

#[derive(Clone, Copy)]
enum Opt {
    Json,
    Line,
    Column,
    IncludeDeclaration,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Json,
        Self::Line,
        Self::Column,
        Self::IncludeDeclaration,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Json => args::OptSpec {
                long: Some("json"),
                short: None,
                value: None,
                desc: "Output as JSON",
            },
            Self::Line => args::OptSpec {
                long: Some("line"),
                short: None,
                value: Some("<n>"),
                desc: "1-based line number for position-based queries",
            },
            Self::Column => args::OptSpec {
                long: Some("column"),
                short: None,
                value: Some("<n>"),
                desc: "1-based column number for position-based queries",
            },
            Self::IncludeDeclaration => args::OptSpec {
                long: Some("include-declaration"),
                short: None,
                value: None,
                desc: "Include the declaration in `references` results",
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
    writeln!(buf, "  diagnostics          Show errors and warnings").unwrap();
    writeln!(
        buf,
        "  references           Find references to the symbol at --line/--column"
    )
    .unwrap();
    writeln!(
        buf,
        "  document-highlight   Highlight occurrences of the symbol at --line/--column"
    )
    .unwrap();
    writeln!(
        buf,
        "  definition           Jump to the definition of the symbol at --line/--column"
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

fn parse_position_value(opt_name: &str, val: String) -> Result<u32, CliExit> {
    val.parse::<u32>().map_err(|_| {
        CliExit::error(format!(
            "{opt_name} requires a positive integer, got '{val}'"
        ))
    })
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
    let mut line: Option<u32> = None;
    let mut column: Option<u32> = None;
    let mut include_declaration = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Json => json = true,
                Opt::Line => {
                    let val = args::require_string(&mut parser)?;
                    line = Some(parse_position_value("--line", val)?);
                }
                Opt::Column => {
                    let val = args::require_string(&mut parser)?;
                    column = Some(parse_position_value("--column", val)?);
                }
                Opt::IncludeDeclaration => include_declaration = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            let val_str = val.to_string_lossy();
            if kind.is_none() {
                kind = Some(match val_str.as_ref() {
                    "diagnostics" => QueryKind::Diagnostics,
                    "references" => QueryKind::References,
                    "document-highlight" => QueryKind::DocumentHighlight,
                    "definition" => QueryKind::Definition,
                    other => {
                        return Err(CliExit::error(format!(
                            "unknown query kind '{other}'. Available: diagnostics, references, document-highlight, definition"
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

    if matches!(
        kind,
        QueryKind::References | QueryKind::DocumentHighlight | QueryKind::Definition
    ) {
        if line.is_none() {
            return Err(CliExit::error_with_usage(
                "--line is required for this query kind",
                &usage,
            ));
        }
        if column.is_none() {
            return Err(CliExit::error_with_usage(
                "--column is required for this query kind",
                &usage,
            ));
        }
    }

    Ok(QueryOptions {
        kind,
        input,
        json,
        line,
        column,
        include_declaration,
    })
}

pub async fn run(opts: QueryOptions) -> Result<(), CliExit> {
    match opts.kind {
        QueryKind::Diagnostics => query_adapter::run_diagnostics(&opts.input, opts.json).await,
        QueryKind::References => {
            query_adapter::run_references(
                &opts.input,
                opts.line.unwrap(),
                opts.column.unwrap(),
                opts.include_declaration,
                opts.json,
            )
            .await
        }
        QueryKind::DocumentHighlight => {
            query_adapter::run_document_highlight(
                &opts.input,
                opts.line.unwrap(),
                opts.column.unwrap(),
                opts.json,
            )
            .await
        }
        QueryKind::Definition => {
            query_adapter::run_definition(
                &opts.input,
                opts.line.unwrap(),
                opts.column.unwrap(),
                opts.json,
            )
            .await
        }
    }
}
