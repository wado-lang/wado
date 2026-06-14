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
    Hover,
}

pub struct QueryOptions {
    kind: QueryKind,
    input: Option<String>,
    json: bool,
    line: Option<u32>,
    column: Option<u32>,
    include_declaration: bool,
    symbol: Option<String>,
    base: Option<String>,
    all: bool,
}

#[derive(Clone, Copy)]
enum Opt {
    Json,
    Line,
    Column,
    IncludeDeclaration,
    Symbol,
    Base,
    All,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Json,
        Self::Line,
        Self::Column,
        Self::IncludeDeclaration,
        Self::Symbol,
        Self::Base,
        Self::All,
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
            Self::Symbol => args::OptSpec {
                long: Some("symbol"),
                short: None,
                value: Some("<notation>"),
                desc: "Locate by symbol notation (e.g. core:json#parse) instead of a position",
            },
            Self::Base => args::OptSpec {
                long: Some("base"),
                short: None,
                value: Some("<dir>"),
                desc: "Base directory for relative modules in --symbol (default: .)",
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
            Self::All => args::OptSpec {
                long: Some("all"),
                short: None,
                value: None,
                desc: "Show private members too (hover/suggestions); default is the public API",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado query <kind> [options] <file.wado>").unwrap();
    writeln!(
        buf,
        "       wado query definition --symbol <notation> [--base <dir>]"
    )
    .unwrap();
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
    writeln!(
        buf,
        "  hover                Show the signature of the symbol at --line/--column"
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

pub fn parse_args(mut parser: lexopt::Parser) -> Result<QueryOptions, CliExit> {
    let usage = format_usage();
    let mut kind: Option<QueryKind> = None;
    let mut input: Option<String> = None;
    let mut json = false;
    let mut line: Option<u32> = None;
    let mut column: Option<u32> = None;
    let mut include_declaration = false;
    let mut symbol: Option<String> = None;
    let mut base: Option<String> = None;
    let mut all = false;

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
                Opt::Symbol => symbol = Some(args::require_string(&mut parser)?),
                Opt::Base => base = Some(args::require_string(&mut parser)?),
                Opt::All => all = true,
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
                    "hover" => QueryKind::Hover,
                    other => {
                        return Err(CliExit::error(format!(
                            "unknown query kind '{other}'. Available: diagnostics, references, document-highlight, definition, hover"
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

    // The `--symbol` locator names its own module, so it takes no input file
    // and no position; it is the alternative to `--line`/`--column`.
    if symbol.is_some() {
        if input.is_some() {
            return Err(CliExit::error_with_usage(
                "--symbol does not take an input file",
                &usage,
            ));
        }
        if line.is_some() || column.is_some() {
            return Err(CliExit::error_with_usage(
                "--symbol cannot be combined with --line/--column",
                &usage,
            ));
        }
        if matches!(kind, QueryKind::Diagnostics) {
            return Err(CliExit::error_with_usage(
                "--symbol is not supported for the `diagnostics` kind",
                &usage,
            ));
        }
        return Ok(QueryOptions {
            kind,
            input,
            json,
            line,
            column,
            include_declaration,
            symbol,
            base,
            all,
        });
    }

    if base.is_some() {
        return Err(CliExit::error_with_usage(
            "--base is only valid together with --symbol",
            &usage,
        ));
    }

    let input = Some(input.ok_or_else(|| CliExit::error_with_usage("missing input file", &usage))?);

    if matches!(
        kind,
        QueryKind::References
            | QueryKind::DocumentHighlight
            | QueryKind::Definition
            | QueryKind::Hover
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
        symbol,
        base,
        all,
    })
}

pub async fn run(opts: QueryOptions) -> Result<(), CliExit> {
    // Default to the public-API view; `--all` opts into private members.
    let public_only = !opts.all;
    if let Some(notation) = &opts.symbol {
        let base = opts.base.as_deref().unwrap_or(".");
        return match opts.kind {
            QueryKind::Definition => {
                query_adapter::run_definition_by_symbol(notation, base, public_only, opts.json)
                    .await
            }
            QueryKind::References => {
                query_adapter::run_references_by_symbol(
                    notation,
                    base,
                    opts.include_declaration,
                    public_only,
                    opts.json,
                )
                .await
            }
            QueryKind::DocumentHighlight => {
                query_adapter::run_document_highlight_by_symbol(
                    notation,
                    base,
                    public_only,
                    opts.json,
                )
                .await
            }
            QueryKind::Hover => {
                query_adapter::run_hover_by_symbol(notation, base, public_only, opts.json).await
            }
            // Rejected during parse_args.
            QueryKind::Diagnostics => unreachable!("--symbol rejected for diagnostics"),
        };
    }

    // Position-based kinds always carry an input file (validated in parse_args).
    let input = opts.input.as_deref().unwrap_or_default();
    match opts.kind {
        QueryKind::Diagnostics => query_adapter::run_diagnostics(input, opts.json).await,
        QueryKind::References => {
            query_adapter::run_references(
                input,
                opts.line.unwrap(),
                opts.column.unwrap(),
                opts.include_declaration,
                opts.json,
            )
            .await
        }
        QueryKind::DocumentHighlight => {
            query_adapter::run_document_highlight(
                input,
                opts.line.unwrap(),
                opts.column.unwrap(),
                opts.json,
            )
            .await
        }
        QueryKind::Definition => {
            query_adapter::run_definition(
                input,
                opts.line.unwrap(),
                opts.column.unwrap(),
                opts.json,
            )
            .await
        }
        QueryKind::Hover => {
            query_adapter::run_hover(
                input,
                opts.line.unwrap(),
                opts.column.unwrap(),
                public_only,
                opts.json,
            )
            .await
        }
    }
}
