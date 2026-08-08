//! `wado lsp` subcommand — delegates to the stdio LSP server in `wado-lsp`.

use std::fmt::Write as _;

use crate::args::{self, CliExit};

#[derive(Debug)]
pub struct LspOptions {}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado lsp [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Start the Wado language server, speaking LSP over stdio."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    writeln!(buf, "  -h, --help  Show this help message").unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<LspOptions, CliExit> {
    let usage = format_usage();
    if let Some(arg) = args::next_arg(&mut parser)? {
        return match arg {
            lexopt::Arg::Long("help") | lexopt::Arg::Short('h') => Err(CliExit::help(usage)),
            other => Err(args::unexpected_arg(other, &usage)),
        };
    }
    Ok(LspOptions {})
}

pub async fn run(_opts: LspOptions) -> Result<(), CliExit> {
    wado_lsp::server::run_stdio().await;
    Ok(())
}
