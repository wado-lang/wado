//! The `wado test` result cache: a file whose tests all passed is neither
//! compiled nor run again while nothing it read has changed.
//!
//! An entry is keyed by the `wado` binary ([`binary_digest`]), the options the
//! run was given, and the file's path, and lists every file its compile read,
//! the stdlib modules it took among them.
//! It is stored only for a component whose outcome those files decide: one
//! importing the filesystem, the network or `core:eval` reads what no compile
//! recorded, and runs every time.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use wado_compiler::hashmap::IndexMap;
use wado_lsp::host::discovery::{absolutize, normalize_path};

use crate::cache::write_atomic;
use crate::compile::build_dir;
use crate::fingerprint::{Input, binary_digest, hash_field};
use crate::kiln_provider::hex32;
use crate::manifest::package_root;
use crate::sync::lock;

/// The import namespaces whose answers no compile records.
const UNRECORDED_IMPORTS: &[&str] = &[
    "wasi:filesystem/",
    "wasi:sockets/",
    "wasi:http/",
    "wasi:tls/",
    "core:eval/",
];

/// Whether a component importing `imports` can be cached: its tests depend on
/// nothing beyond what its compile read.
pub(crate) fn cacheable<'a>(mut imports: impl Iterator<Item = &'a str>) -> bool {
    imports.all(|name| {
        !UNRECORDED_IMPORTS
            .iter()
            .any(|prefix| name.starts_with(prefix))
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
    /// it.
    on_disk: std::sync::Mutex<IndexMap<PathBuf, Option<Input>>>,
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
            on_disk: std::sync::Mutex::default(),
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

    /// How `file`'s tests went when it last ran, if nothing it read has
    /// changed since.
    pub(crate) fn lookup(&self, file: &str) -> Option<Tally> {
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
        let current = lock(&self.on_disk).get(&input.path).cloned();
        let current = current.unwrap_or_else(|| {
            let read = Input::on_disk(input.path.clone());
            lock(&self.on_disk).insert(input.path.clone(), read.clone());
            read
        });
        current.as_ref() == Some(input)
    }

    /// Record that `file`, having read `inputs`, passed as `tally` says.
    pub(crate) fn store(&self, file: &str, inputs: Vec<Input>, tally: Tally) {
        let bytes = serde_json::to_vec(&Entry { inputs, tally }).expect("an entry serializes");
        // Losing the cache is never an error: the next run tests again.
        let _ = write_atomic(&self.entry_path(Path::new(file)), &bytes);
    }
}
