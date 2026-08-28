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
    InlayHints,
}

impl QueryKind {
    /// Every kind, in the order the help lists them. The parse arm, the help
    /// text, and the "Available:" error all read from here.
    const ALL: &[Self] = &[
        Self::Diagnostics,
        Self::References,
        Self::DocumentHighlight,
        Self::Definition,
        Self::Hover,
        Self::InlayHints,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Diagnostics => "diagnostics",
            Self::References => "references",
            Self::DocumentHighlight => "document-highlight",
            Self::Definition => "definition",
            Self::Hover => "hover",
            Self::InlayHints => "inlay-hints",
        }
    }

    const fn desc(self) -> &'static str {
        match self {
            Self::Diagnostics => "Show errors and warnings",
            Self::References => "Find references to the symbol at --line/--column",
            Self::DocumentHighlight => "Highlight occurrences of the symbol at --line/--column",
            Self::Definition => "Jump to the definition of the symbol at --line/--column",
            Self::Hover => "Show the signature of the symbol at --line/--column",
            Self::InlayHints => "Show the inferred-type and parameter-name hints, in the source",
        }
    }

    fn parse(name: &str) -> Result<Self, CliExit> {
        Self::ALL
            .iter()
            .find(|kind| kind.name() == name)
            .copied()
            .ok_or_else(|| {
                let available: Vec<&str> = Self::ALL.iter().map(|k| k.name()).collect();
                CliExit::error(format!(
                    "unknown query kind '{name}'. Available: {}",
                    available.join(", ")
                ))
            })
    }
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
    for kind in QueryKind::ALL {
        writeln!(buf, "  {:<20} {}", kind.name(), kind.desc()).unwrap();
    }
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
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
                kind = Some(QueryKind::parse(val_str.as_ref())?);
            } else {
                args::reject_multiple_inputs(&input)?;
                input = Some(val_str.into_owned());
            }
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let kind = kind.ok_or_else(|| CliExit::error_with_usage("missing query kind", &usage))?;

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
        if matches!(kind, QueryKind::Diagnostics | QueryKind::InlayHints) {
            return Err(CliExit::error_with_usage(
                "--symbol is not supported for the `diagnostics` / `inlay-hints` kinds",
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
            QueryKind::Diagnostics | QueryKind::InlayHints => {
                unreachable!("--symbol rejected for diagnostics / inlay-hints")
            }
        };
    }

    // Position-based kinds always carry an input file (validated in parse_args).
    let input = opts.input.as_deref().unwrap_or_default();
    match opts.kind {
        QueryKind::Diagnostics => query_adapter::run_diagnostics(input, opts.json).await,
        QueryKind::InlayHints => query_adapter::run_inlay_hints(input, opts.json).await,
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
