//! The `wado test` result cache: a file whose tests all passed is neither
//! compiled nor run again while nothing it read has changed.
//!
//! An entry is keyed by the `wado` binary ([`binary_digest`]), the options the
//! run was given, and the file's path, and lists every file its compile read,
//! the stdlib modules it took among them.
//! It is stored only for a component whose outcome those files decide: one
//! importing any effect beyond output receives what no compile recorded, and
//! runs every time.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use wado_compiler::hashmap::IndexMap;
use wado_lsp::host::discovery::{absolutize, normalize_path};

use crate::compile::build_dir;
use crate::fingerprint::{Input, binary_digest, hash_field};
use crate::kiln_provider::hex32;
use crate::manifest::package_root;
use wado_host::atomic_write::write_atomic;
use wado_host::sync::lock;

/// The interfaces that hand a cached run nothing its key does not decide:
/// output, its types, exit, and `core:fuel`, whose `spent` answers `None`
/// since a metered run is never cached. Every other import is an effect whose
/// answer can change between runs (a clock, randomness, a file, the network,
/// `core:eval`), so it decides the outcome as much as the compile's inputs do.
const OUTCOME_NEUTRAL_IMPORTS: &[&str] = &[
    "wasi:cli/types",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/exit",
    "core:fuel/fuel-host",
];

/// Whether a component importing `imports` can be cached: its tests depend on
/// nothing beyond what its compile read.
pub(crate) fn cacheable<'a>(mut imports: impl Iterator<Item = &'a str>) -> bool {
    imports.all(|name| {
        let unversioned = name
            .split_once('@')
            .map_or(name, |(interface, _)| interface);
        OUTCOME_NEUTRAL_IMPORTS.contains(&unversioned)
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    inputs: Vec<Input>,
    tally: Tally,
}

/// A cached file's tests, counted again on a hit.
#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Tally {
    pub(crate) passed: u32,
    /// `#[TODO]` tests that trapped, as expected.
    pub(crate) todo_pending: u32,
}

impl Tally {
    /// What a report of a cached file says about it.
    pub(crate) fn describe(self) -> String {
        let mut text = format!("{} passed", self.passed);
        if self.todo_pending > 0 {
            text.push_str(&format!(", {} todo", self.todo_pending));
        }
        text.push_str(" before; nothing it read changed");
        text
    }
}

pub(crate) struct TestCache {
    /// The binary and the run's options.
    digest: [u8; 32],
    /// `--no-cache`: store, but never answer from, the cache.
    read: bool,
    /// What each input holds now, read once per run however many entries list
    /// it, by its path and the prefix recorded of it.
    on_disk: Mutex<IndexMap<(PathBuf, Option<u64>), Option<Input>>>,
}

impl TestCache {
    /// `options` renders everything about the run that can change an outcome.
    pub(crate) fn new(options: &str, read: bool) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(binary_digest());
        hash_field(&mut hasher, options.as_bytes());
        Self {
            digest: hasher.finalize().into(),
            read,
            on_disk: Mutex::default(),
        }
    }

    /// `build/test/` under the file's package root, or under its directory
    /// when it is in no package.
    fn entry_path(&self, file: &Path) -> PathBuf {
        let file = normalize_path(&absolutize(file));
        let dir = file.parent().expect("a test file has a parent directory");
        let root = package_root(dir).unwrap_or_else(|| dir.to_path_buf());
        let mut hasher = Sha256::new();
        hasher.update(self.digest);
        hash_field(&mut hasher, file.as_os_str().as_encoded_bytes());
        let key: [u8; 32] = hasher.finalize().into();
        build_dir(&root)
            .join("test")
            .join(format!("{}.json", hex32(&key)))
    }

    /// Take out of `files` each one whose tests passed when it last ran and
    /// that nothing it read has changed since, with how they went.
    pub(crate) fn take_hits(&self, files: &mut Vec<String>) -> Vec<(String, Tally)> {
        let mut hits = Vec::new();
        files.retain(|file| match self.lookup(file) {
            Some(tally) => {
                hits.push((file.clone(), tally));
                false
            }
            None => true,
        });
        hits
    }

    fn lookup(&self, file: &str) -> Option<Tally> {
        if !self.read {
            return None;
        }
        let bytes = std::fs::read(self.entry_path(Path::new(file))).ok()?;
        let entry: Entry = serde_json::from_slice(&bytes).ok()?;
        entry
            .inputs
            .iter()
            .all(|input| self.unchanged(input))
            .then_some(entry.tally)
    }

    fn unchanged(&self, input: &Input) -> bool {
        lock(&self.on_disk)
            .entry((input.path.clone(), input.prefix))
            .or_insert_with(|| Input::on_disk(input.path.clone(), input.prefix))
            .as_ref()
            == Some(input)
    }

    /// Record that `file`, having read `inputs`, passed as `tally` says.
    pub(crate) fn store(&self, file: &str, inputs: Vec<Input>, tally: Tally) {
        let bytes = serde_json::to_vec(&Entry { inputs, tally }).expect("an entry serializes");
        // Losing the cache is never an error: the next run tests again.
        let _ = write_atomic(&self.entry_path(Path::new(file)), &bytes);
    }
}
