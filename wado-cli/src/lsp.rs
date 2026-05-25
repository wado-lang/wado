//! `wado lsp` subcommand — delegates to the stdio LSP server in `wado-lsp`.

use crate::args::CliExit;

pub async fn run() -> Result<(), CliExit> {
    wado_lsp::server::run_stdio().await;
    Ok(())
}
