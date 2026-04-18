//! Stdio LSP server entrypoint. Builds for both native and `wasm32-wasip2`.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    wado_lsp::server::run_stdio().await;
}
