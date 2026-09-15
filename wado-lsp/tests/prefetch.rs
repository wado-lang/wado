//! The prefetch seam: a pinned-but-cold registry dependency is handed to the
//! installed prefetcher while the resolution itself stays offline.
//!
//! Its own test binary: the prefetcher is process-global and the cache root is
//! read from the environment.

use std::sync::{Arc, Mutex};

use wado_lsp::host::discovery::resolve_all;
use wado_lsp::host::prefetch;

#[test]
fn a_cold_pin_is_handed_to_the_prefetcher_and_still_reads_unresolved() {
    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: single-threaded test, and the whole binary is this one test.
    unsafe { std::env::set_var("WADO_ROOT", tmp.path().join("cache")) };

    let project = tmp.path().join("app");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("wado.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [registries]\ndefault = \"oci://ghcr.io\"\n\n\
         [dependencies]\n\"wado-lang:marl\" = { version = \"^0.1\" }\n",
    )
    .unwrap();
    std::fs::write(
        project.join("wado.lock"),
        "version = 1\ndeps-hash = \"sha256:0\"\n\n[[package]]\n\
         id = \"registry+oci://ghcr.io/wado-lang:marl\"\n\
         version = \"0.1.2\"\ndeps = []\n",
    )
    .unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    prefetch::set_prefetcher(move |needs| {
        recorder
            .lock()
            .unwrap()
            .extend(needs.into_iter().map(|n| (n.coordinate, n.version)));
    });

    let manifest = std::fs::read_to_string(project.join("wado.toml")).unwrap();
    let manifest: wado_manifest::Manifest = manifest.parse().unwrap();
    let resolved = resolve_all(&manifest, &project);

    assert_eq!(
        *seen.lock().unwrap(),
        [("wado-lang:marl".to_string(), "0.1.2".to_string())],
    );
    let (name, entry) = resolved.first().expect("one dependency");
    assert_eq!(name, "wado-lang:marl");
    let reason = entry.as_ref().expect_err("cold cache is unresolved");
    assert!(reason.contains("wado fetch"), "{reason}");
}
