//! `wado lsp` subcommand — delegates to the stdio LSP server in `wado-lsp`.

use std::fmt::Write as _;
use std::sync::Mutex;

use wado_compiler::hashmap::IndexSet;

use wado_manifest::RegistryComponentNeed;

use crate::args::{self, CliExit};
use crate::dep_component::pull_component;
use crate::sync::lock;

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

/// Warm the component cache for the language server: every pinned-but-cold
/// registry dependency the offline resolution reports is pulled on a background
/// task, so the next request resolves it while this one answers immediately.
///
/// One attempt per `coordinate@version` per process: a failed pull (offline,
/// unauthorized) must not be retried on every keystroke, and a successful one
/// leaves the file in the cache where discovery finds it.
fn install_prefetcher() {
    let attempted: Mutex<IndexSet<String>> = Mutex::new(IndexSet::default());
    wado_lsp::host::prefetch::set_prefetcher(move |needs: Vec<RegistryComponentNeed>| {
        for need in needs {
            if !lock(&attempted).insert(format!("{}@{}", need.coordinate, need.version)) {
                continue;
            }
            tokio::spawn(async move {
                let _ = pull_component(&need.registry_url, &need.coordinate, &need.version).await;
            });
        }
    });
}

pub async fn run(_opts: LspOptions) -> Result<(), CliExit> {
    install_prefetcher();
    match wado_lsp::server::run_stdio().await {
        0 => Ok(()),
        code => Err(CliExit::silent_failure(code)),
    }
}
