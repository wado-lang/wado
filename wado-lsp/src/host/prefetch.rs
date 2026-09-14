//! The seam a network-capable host warms the dependency cache through.
//!
//! Resolution here is offline, so a pinned-but-cold dependency is handed over
//! instead of waited on. `wado lsp` installs one; the browser installs none.

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
    if let Some(prefetcher) = PREFETCHER.get() {
        prefetcher(needs);
    }
}
