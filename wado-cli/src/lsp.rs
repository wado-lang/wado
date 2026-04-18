//! `wado lsp` subcommand — delegates to the stdio LSP server in `wado-lsp`.

/// Run the LSP server over stdio.
pub async fn run() {
    wado_lsp::server::run_stdio().await;
}
