//! A cache of what a compile of one file produced, valid while nothing the
//! compile read has changed. `wado test` keeps a pass in one, `wado run` a
//! component.
//!
//! An entry is named by the options the run was given and the file's path, and
//! holds the `wado` binary it was made by ([`binary_digest`]) and every file
//! and environment variable its compile read, the stdlib modules it took among
//! them. A rebuilt binary therefore replaces the entry rather than adding one
//! beside it. It lives in the file's [`derived_dir`].

use std::ffi::OsString;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use wado_compiler::hashmap::IndexMap;
use wado_lsp::host::discovery::{absolutize, normalize_path};

use crate::cache::derived_dir;
use crate::compile::CompileFlags;
use crate::fingerprint::{Input, InputKey, binary_digest, hash_field, write_entry};
use crate::kiln_provider::hex32;
use crate::manifest::package_root;
use wado_host::sync::lock;

/// The `WADO_*` environment, sorted. The compiler reads these itself
/// (`WADO_SKIP_PASS`, say) rather than through its host,
/// and they change what it emits.
fn wado_env() -> Vec<(OsString, OsString)> {
    let mut vars: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| name.as_encoded_bytes().starts_with(b"WADO_"))
        .collect();
    vars.sort();
    vars
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Entry<T> {
    binary: String,
    inputs: Vec<Input>,
    value: T,
}

pub(crate) struct InputCache {
    /// Names the [`derived_dir`] entries go in.
    kind: &'static str,
    /// The run's options.
    options: [u8; 32],
    binary: String,
    /// `--no-cache`: store, but never answer from, the cache.
    read: bool,
    /// What each input holds now, read once per run however many entries list
    /// it.
    now: Mutex<IndexMap<InputKey, Option<Input>>>,
}

impl InputCache {
    /// `flags` and `context` render everything about the run beyond the
    /// compile's reads that can change an outcome.
    pub(crate) fn new(kind: &'static str, flags: &CompileFlags, context: impl Debug) -> Self {
        let mut keyed = flags.clone();
        // `--no-cache` only skips reading, so it is no part of the key.
        keyed.knobs.no_cache = false;
        let options = format!("{keyed:?} {context:?} {:?}", wado_env());
        Self {
            kind,
            options: Sha256::digest(options.as_bytes()).into(),
            binary: hex32(&binary_digest()),
            read: !flags.knobs.no_cache,
            now: Mutex::default(),
        }
    }

    /// `file`'s entry; `None` where no directory can hold it.
    pub(crate) fn slot(&self, file: &Path) -> Option<Slot<'_>> {
        let file = normalize_path(&absolutize(file));
        let dir = file.parent().expect("a source file has a parent directory");
        let mut hasher = Sha256::new();
        hasher.update(self.options);
        hash_field(&mut hasher, file.as_os_str().as_encoded_bytes());
        let key: [u8; 32] = hasher.finalize().into();
        let stem = derived_dir(package_root(dir).as_deref(), self.kind)?.join(hex32(&key));
        Some(Slot { cache: self, stem })
    }

    /// What `file` produced when it last compiled, where nothing it read has
    /// changed since.
    pub(crate) fn lookup<T: DeserializeOwned>(&self, file: &Path) -> Option<T> {
        self.slot(file)?.lookup()
    }

    fn unchanged(&self, input: &Input) -> bool {
        let key = input.key();
        lock(&self.now)
            .entry(key.clone())
            .or_insert_with(|| Input::now(&key))
            .as_ref()
            == Some(input)
    }

    /// Record that `file`, having read `inputs`, produced `value`.
    pub(crate) fn store<T: Serialize>(&self, file: &Path, inputs: Vec<Input>, value: T) {
        if let Some(slot) = self.slot(file) {
            slot.store(inputs, value);
        }
    }
}

/// Where one file's entry lives, found once for a lookup and a store.
pub(crate) struct Slot<'a> {
    cache: &'a InputCache,
    stem: PathBuf,
}

impl Slot<'_> {
    /// A file kept beside the entry, under `extension`.
    pub(crate) fn path(&self, extension: &str) -> PathBuf {
        self.stem.with_extension(extension)
    }

    pub(crate) fn lookup<T: DeserializeOwned>(&self) -> Option<T> {
        if !self.cache.read {
            return None;
        }
        let bytes = std::fs::read(self.path("json")).ok()?;
        let entry: Entry<T> = serde_json::from_slice(&bytes).ok()?;
        (entry.binary == self.cache.binary
            && entry.inputs.iter().all(|input| self.cache.unchanged(input)))
        .then_some(entry.value)
    }

    pub(crate) fn store<T: Serialize>(&self, inputs: Vec<Input>, value: T) {
        let binary = self.cache.binary.clone();
        write_entry(
            &self.path("json"),
            &Entry {
                binary,
                inputs,
                value,
            },
        );
    }
}
