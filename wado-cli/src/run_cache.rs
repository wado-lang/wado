//! Per-run shared state: what a single CLI invocation resolves once and then
//! holds fixed for its whole duration.
//!
//! A `wado test` run compiles thousands of files over minutes. Without a
//! run-scoped view, every fixture re-resolves the same generator from disk, so
//! editing a generator source mid-run silently splits the run: fixtures
//! compiled before the edit used one generator, fixtures after it another, and
//! the summary describes a tree that never existed. [`GeneratorCache`] pins the
//! resolution for the run, and [`SourceWatch`] records what the run read so it
//! can say afterwards whether the tree moved under it.
//!
//! Attached explicitly by the run (`with_shared_run_cache`); a bare host or
//! provider keeps per-call semantics, which is what a single-file `wado
//! compile` and the unit tests want.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use wado_compiler::hashmap::IndexMap;

use crate::compiler_host::KilnComponentCache;
use crate::kiln_driver::ResolvedGenerator;

/// A slot map is structurally valid whatever a panicking holder was doing, so
/// recovering the guard is correct, not a fallback.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Everything one CLI run shares across the files it processes.
#[derive(Default)]
pub struct RunCache {
    components: KilnComponentCache,
    generators: GeneratorCache,
    inputs: SourceWatch,
}

impl std::fmt::Debug for RunCache {
    /// The contents are caches keyed by content hashes; naming them says
    /// nothing a reader of a `{:?}` dump can use.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunCache")
    }
}

impl RunCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn components(&self) -> &KilnComponentCache {
        &self.components
    }

    #[must_use]
    pub fn generators(&self) -> &GeneratorCache {
        &self.generators
    }

    #[must_use]
    pub fn inputs(&self) -> &SourceWatch {
        &self.inputs
    }
}

/// Generators resolved by this run, keyed by manifest root + module.
///
/// The first resolution wins for the rest of the run: a generator edited while
/// the run is in flight cannot take effect halfway through it, which is what
/// makes a run's results describe one tree. [`SourceWatch`] is what tells the
/// user the edit happened.
#[derive(Default)]
pub struct GeneratorCache {
    resolved: Mutex<IndexMap<String, ResolvedGenerator>>,
}

impl GeneratorCache {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<ResolvedGenerator> {
        lock(&self.resolved).get(key).cloned()
    }

    /// Store `resolved` under `key` unless the run already pinned one, and
    /// return whatever is pinned afterwards — so racing resolutions of the same
    /// generator all observe the same answer.
    pub fn pin(&self, key: String, resolved: ResolvedGenerator) -> ResolvedGenerator {
        lock(&self.resolved).entry(key).or_insert(resolved).clone()
    }
}

/// What each watched file looked like when the run first read it.
#[derive(Clone, Debug)]
struct Stamp {
    len: u64,
    hash: [u8; 32],
    /// A later read in the same run returned different bytes. Sticky, because
    /// an edit reverted before the run ends still means two fixtures compiled
    /// two different files — which the end-of-run comparison cannot see.
    diverged: bool,
}

/// The source files a run read, with what they contained at first read.
///
/// [`Self::changed`] answers "did anything move under us": a file whose content
/// differs from the first read means the run mixed two trees, so its verdict is
/// about neither. Content, not timestamps — a save that rewrites identical
/// bytes, and the run's own repeated writes of a deterministic artefact, are
/// not changes.
#[derive(Default)]
pub struct SourceWatch {
    seen: Mutex<IndexMap<PathBuf, Stamp>>,
    /// Kiln output directories. Everything under one is a product of this run,
    /// not an input to it, so a regenerated file is not a moving tree. Applied
    /// at [`Self::changed`] too, since a directory is declared by the file that
    /// invokes the generator, which may compile after one that reads its
    /// output.
    generated_dirs: Mutex<Vec<PathBuf>>,
}

impl SourceWatch {
    /// Record `bytes` as what `path` held when the run first read it. Later
    /// reads are ignored: the question is whether the file still matches the
    /// run's first view of it, not its most recent one.
    pub fn observe(&self, path: &Path, bytes: &[u8]) {
        // Hash outside the lock: `wado test` reads files from every compile
        // worker at once. The insert-or-compare below then happens under one
        // lock hold, so two workers reading the same path concurrently cannot
        // each decide they are the first and drop the loser's hash.
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let mut seen = lock(&self.seen);
        match seen.get_mut(path) {
            Some(stamp) => stamp.diverged |= stamp.hash != hash,
            None => {
                seen.insert(
                    path.to_path_buf(),
                    Stamp {
                        len: bytes.len() as u64,
                        hash,
                        diverged: false,
                    },
                );
            }
        }
    }

    /// Declare `dir` a Kiln output directory, whose files this run writes.
    pub fn mark_generated_dir(&self, dir: PathBuf) {
        let mut dirs = lock(&self.generated_dirs);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }

    /// Watched files whose content no longer matches the run's first read of
    /// them — either at some read during the run, or on disk now. Sorted for a
    /// stable report. A file that vanished counts: the run compiled something
    /// that is no longer there.
    #[must_use]
    pub fn changed(&self) -> Vec<PathBuf> {
        let generated = lock(&self.generated_dirs).clone();
        let seen = lock(&self.seen).clone();
        let mut changed: Vec<PathBuf> = seen
            .into_iter()
            .filter(|(path, _)| !generated.iter().any(|dir| path.starts_with(dir)))
            .filter(|(path, stamp)| stamp.diverged || moved(path, stamp))
            .map(|(path, _)| path)
            .collect();
        changed.sort();
        changed
    }
}

/// Whether `path` differs from `stamp`. Length is the one cheap filter that
/// cannot be wrong; a timestamp can, because the bytes reach [`SourceWatch`]
/// already read, so a write between the read and the stat would pair the old
/// content with the new mtime and hide the change for the rest of the run.
/// Everything else is decided by re-reading, so a rewrite of identical bytes
/// reports nothing.
fn moved(path: &Path, stamp: &Stamp) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() != stamp.len => return true,
        Ok(_) => {}
        Err(_) => return true,
    }
    match std::fs::read(path) {
        Ok(bytes) => <[u8; 32]>::from(Sha256::digest(&bytes)) != stamp.hash,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wado-run-cache-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("write");
    }

    #[test]
    fn an_unchanged_file_is_not_reported() {
        let dir = tmp_dir("unchanged");
        let file = dir.join("a.wado");
        write(&file, "fn a() {}");
        let watch = SourceWatch::default();
        watch.observe(&file, b"fn a() {}");
        assert!(watch.changed().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_edited_after_the_first_read_is_reported() {
        let dir = tmp_dir("edited");
        let file = dir.join("a.wado");
        write(&file, "fn a() {}");
        let watch = SourceWatch::default();
        watch.observe(&file, b"fn a() {}");
        write(&file, "fn a() { let x = 1; }");
        assert_eq!(watch.changed(), vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rewrite_of_identical_bytes_is_not_a_change() {
        // An editor saving an untouched buffer, and the run rewriting its own
        // deterministic output, both land here.
        let dir = tmp_dir("identical");
        let file = dir.join("a.wado");
        write(&file, "fn a() {}");
        let watch = SourceWatch::default();
        watch.observe(&file, b"fn a() {}");
        write(&file, "fn a() {}");
        assert!(watch.changed().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_file_is_reported() {
        let dir = tmp_dir("deleted");
        let file = dir.join("a.wado");
        write(&file, "fn a() {}");
        let watch = SourceWatch::default();
        watch.observe(&file, b"fn a() {}");
        std::fs::remove_file(&file).expect("remove");
        assert_eq!(watch.changed(), vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_regenerated_kiln_output_is_not_a_changed_input() {
        let dir = tmp_dir("generated");
        let out_dir = dir.join("generated");
        std::fs::create_dir_all(&out_dir).expect("out dir");
        let file = out_dir.join("g.wado");
        write(&file, "// v1");
        let watch = SourceWatch::default();
        watch.observe(&file, b"// v1");
        // The declaring file compiles later than one that read the output.
        write(&file, "// v2");
        watch.mark_generated_dir(out_dir);
        assert!(watch.changed().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_reverted_before_the_run_ends_is_still_reported() {
        // Two fixtures compiled two different files; that the tree looks
        // untouched afterwards does not put the run back together.
        let dir = tmp_dir("reverted");
        let file = dir.join("a.wado");
        write(&file, "v1");
        let watch = SourceWatch::default();
        watch.observe(&file, b"v1");
        watch.observe(&file, b"v2");
        write(&file, "v1");
        assert_eq!(watch.changed(), vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_racing_the_read_is_not_hidden_by_a_matching_size() {
        // The bytes reach the watch already read, so the file on disk can
        // already be the next version by the time the stamp is taken. Same
        // length, so only comparing content catches it.
        let dir = tmp_dir("read-race");
        let file = dir.join("a.wado");
        write(&file, "NEW");
        let watch = SourceWatch::default();
        watch.observe(&file, b"OLD");
        assert_eq!(watch.changed(), vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_first_reads_of_different_bytes_diverge() {
        // Two compile workers reach the same path at once, on either side of an
        // edit. Whichever stamps it first, the other must be compared against
        // it rather than deciding it is also the first.
        let dir = tmp_dir("concurrent-first");
        let file = dir.join("a.wado");
        write(&file, "v1");
        let watch = SourceWatch::default();
        std::thread::scope(|scope| {
            for version in ["v1", "v2"] {
                scope.spawn(|| watch.observe(&file, version.as_bytes()));
            }
        });
        assert_eq!(watch.changed(), vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeated_reads_of_the_same_bytes_are_not_a_change() {
        let dir = tmp_dir("repeat-read");
        let file = dir.join("a.wado");
        write(&file, "v1");
        let watch = SourceWatch::default();
        watch.observe(&file, b"v1");
        watch.observe(&file, b"v1");
        assert!(watch.changed().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
