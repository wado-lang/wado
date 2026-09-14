//! The seam a network-capable host uses to warm the dependency cache.
//!
//! Resolution here is offline: a registry dependency resolves from the cache or
//! not at all. `wado lsp` installs a prefetcher so a pinned-but-cold dependency
//! is pulled in the background and the next request resolves it — an editor
//! request itself never waits on the network. The browser installs none.

use std::sync::OnceLock;

use wado_manifest::RegistryComponentNeed;

type Prefetcher = Box<dyn Fn(Vec<RegistryComponentNeed>) + Send + Sync>;

static PREFETCHER: OnceLock<Prefetcher> = OnceLock::new();

/// Install the process's prefetcher. The first install wins, so a second call
/// (a nested server, a test) leaves the first in place.
pub fn set_prefetcher(f: impl Fn(Vec<RegistryComponentNeed>) + Send + Sync + 'static) {
    let _ = PREFETCHER.set(Box::new(f));
}

/// Hand `needs` to the installed prefetcher, if any. Returns immediately: what
/// the prefetcher does with them is its own business.
pub(crate) fn request(needs: Vec<RegistryComponentNeed>) {
    if needs.is_empty() {
        return;
    }
    if let Some(prefetcher) = PREFETCHER.get() {
        prefetcher(needs);
    }
}
