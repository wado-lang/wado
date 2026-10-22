//! `wado lsp` subcommand — delegates to the stdio LSP server in `wado-lsp`.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::pin::pin;
use std::sync::Mutex;

use futures::future::{Either, select};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use wado_compiler::hashmap::IndexSet;
use wado_manifest::RegistryComponentNeed;

use crate::args::{self, CliExit};
use crate::dep_component::pull_component;
use wado_host::sync::{joined, lock, next_joined};

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

/// Hand each pinned-but-cold registry dependency to [`prefetch`], so the next
/// request resolves it while this one answers immediately.
///
/// One attempt per cache entry per process: a pull that fails (offline,
/// unauthorized) must not be retried on every keystroke. The registry is part of
/// the identity, as it is of the cache path two registries serving one
/// `coordinate@version` land on.
fn install_prefetcher(pulls: mpsc::UnboundedSender<RegistryComponentNeed>) {
    let attempted: Mutex<IndexSet<(String, String, String)>> = Mutex::new(IndexSet::default());
    wado_lsp::host::prefetch::set_prefetcher(move |needs: Vec<RegistryComponentNeed>| {
        for need in needs {
            let entry = (
                need.registry_url.clone(),
                need.coordinate.clone(),
                need.version.clone(),
            );
            if lock(&attempted).insert(entry) {
                pulls
                    .send(need)
                    .expect("the prefetch loop outlives the server");
            }
        }
    });
}

/// Pull each need as it arrives, warning of a pull that fails. A pull's panic
/// continues here.
async fn prefetch(mut needs: mpsc::UnboundedReceiver<RegistryComponentNeed>) -> Infallible {
    let mut pulls = JoinSet::new();
    loop {
        let need = {
            let next = pin!(needs.recv());
            let ended = pin!(next_joined(&mut pulls));
            match select(next, ended).await {
                Either::Left((need, _)) => need.expect("the prefetcher holds the sender for good"),
                Either::Right(((), _)) => continue,
            }
        };
        pulls.spawn(async move {
            let pulled = pull_component(&need.registry_url, &need.coordinate, &need.version).await;
            if let Err(e) = pulled {
                eprintln!(
                    "warning: prefetching {}@{} from {}: {e}",
                    need.coordinate, need.version, need.registry_url
                );
            }
        });
    }
}

pub async fn run(_opts: LspOptions) -> Result<(), CliExit> {
    let (needs_tx, needs) = mpsc::unbounded_channel();
    install_prefetcher(needs_tx);
    // The server blocks its thread on stdin, so it gets one of its own and
    // this task stays free to watch the pulls.
    let server =
        tokio::task::spawn_blocking(|| Handle::current().block_on(wado_lsp::server::run_stdio()));
    let code = match select(server, pin!(prefetch(needs))).await {
        Either::Left((code, _)) => joined(code),
        Either::Right((never, _)) => match never {},
    };
    match code {
        0 => Ok(()),
        code => Err(CliExit::silent_failure(code)),
    }
}
