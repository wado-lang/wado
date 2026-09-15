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

/// Pull each pinned-but-cold registry dependency on a background task, so the
/// next request resolves it while this one answers immediately.
///
/// One attempt per cache entry per process: a pull that fails (offline,
/// unauthorized) must not be retried on every keystroke. The registry is part of
/// the identity, as it is of the cache path two registries serving one
/// `coordinate@version` land on.
fn install_prefetcher() {
    let attempted: Mutex<IndexSet<(String, String, String)>> = Mutex::new(IndexSet::default());
    wado_lsp::host::prefetch::set_prefetcher(move |needs: Vec<RegistryComponentNeed>| {
        for need in needs {
            let entry = (
                need.registry_url.clone(),
                need.coordinate.clone(),
                need.version.clone(),
            );
            if !lock(&attempted).insert(entry) {
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
