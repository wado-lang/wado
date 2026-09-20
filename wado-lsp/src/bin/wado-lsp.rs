//! Stdio LSP server entrypoint. Builds for both native and `wasm32-wasip2`.

fn main() {
    wado_lsp::host::install_dev_stdlib();
    std::process::exit(futures::executor::block_on(wado_lsp::server::run_stdio()));
}
