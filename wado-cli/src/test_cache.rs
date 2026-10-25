//! The `wado test` result cache: a file whose tests all passed is neither
//! compiled nor run again while nothing it read has changed, an
//! [`InputCache`] entry.
//!
//! It is stored only for a component whose outcome those files decide: one
//! importing any effect beyond output receives what no compile recorded, and
//! runs every time.

use std::path::Path;

use crate::fingerprint::Input;
use crate::input_cache::InputCache;

/// The interfaces that hand a test nothing: output, its types, and exit.
/// Every other import is an effect whose answer can change between runs (a
/// clock, randomness, a file, the network, `core:eval`), so it decides the
/// outcome as much as the compile's inputs do.
const OUTPUT_ONLY_IMPORTS: &[&str] = &[
    "wasi:cli/types",
    "wasi:cli/stdout",
    "wasi:cli/stderr",
    "wasi:cli/exit",
];

/// Whether a component importing `imports` can be cached: its tests depend on
/// nothing beyond what its compile read.
pub(crate) fn cacheable<'a>(mut imports: impl Iterator<Item = &'a str>) -> bool {
    imports.all(|name| {
        let unversioned = name
            .split_once('@')
            .map_or(name, |(interface, _)| interface);
        OUTPUT_ONLY_IMPORTS.contains(&unversioned)
    })
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

pub(crate) struct TestCache(InputCache);

impl TestCache {
    /// `options` renders everything about the run that can change an outcome.
    pub(crate) fn new(options: &str, read: bool) -> Self {
        Self(InputCache::new("test", options, read))
    }

    /// Take out of `files` each one whose tests passed when it last ran and
    /// that nothing it read has changed since, with how they went.
    pub(crate) fn take_hits(&self, files: &mut Vec<String>) -> Vec<(String, Tally)> {
        let mut hits = Vec::new();
        files.retain(|file| match self.0.lookup(Path::new(file)) {
            Some(tally) => {
                hits.push((file.clone(), tally));
                false
            }
            None => true,
        });
        hits
    }

    /// Record that `file`, having read `inputs`, passed as `tally` says.
    pub(crate) fn store(&self, file: &str, inputs: Vec<Input>, tally: Tally) {
        self.0.store(Path::new(file), inputs, tally);
    }
}
