//! `wado lsp` subcommand — delegates to the stdio LSP server in `wado-lsp`.

use crate::args::CliExit;

/// Run the LSP server over stdio.
///
/// # Errors
///
/// Currently infallible — `wado_lsp::server::run_stdio` returns `()`. The
/// `Result` shape exists so the `lsp` subcommand's signature matches every
/// other subcommand and propagates uniformly through `main::dispatch`.
pub async fn run() -> Result<(), CliExit> {
    wado_lsp::server::run_stdio().await;
    Ok(())
}
