//! Writing a file so no reader ever sees it half-written.

use std::io;
use std::path::Path;

const STAGING_INFIX: &str = ".tmp-wado-";

/// The sibling [`write_atomic`] stages `file_name` in, unique per process and
/// per thread so two writers never rename away each other's temp.
pub fn staging_file_name(file_name: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    format!(
        "{file_name}{STAGING_INFIX}{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

#[must_use]
pub fn is_staging_file(file_name: &str) -> bool {
    file_name.contains(STAGING_INFIX)
}

/// Write `bytes` to `path` atomically. A crash or a concurrent writer can only
/// leave a stray staging file, never a half-written one the `is_file()` cache
/// check would then trust forever.
///
/// # Errors
///
/// Fails when the parent directory cannot be created or the file cannot be
/// written or renamed into place.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(staging_file_name(
        &path.file_name().unwrap_or_default().to_string_lossy(),
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::write_atomic;

    #[test]
    fn write_atomic_is_concurrency_safe_for_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghcr.io/ns/pkg/0.1.0/component.wasm");
        let bytes: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

        const THREADS: usize = 32;
        let barrier = std::sync::Barrier::new(THREADS);
        let results = std::thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        write_atomic(&path, &bytes)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });

        for r in &results {
            assert!(r.is_ok(), "a concurrent write_atomic failed: {r:?}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
