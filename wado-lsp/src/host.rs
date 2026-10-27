//! Filesystem-based `CompilerHost` for embedders that need silent diagnostic
//! collection plus relative-path source loading.
//!
//! This is the default host used by the LSP server. Consumers that additionally
//! want to decorate output (timestamps, log-level filtering, stderr printing)
//! wrap this host. [`discovery`] holds the filesystem reads that answer what
//! this host's [`CompilerHost::dependency_index`] reports.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use wado_compiler::remote::Integrity;
use wado_compiler::{
    CompilerHost, DependencyIndex, DependencyManifest, Diagnostic, Severity, SourceError,
    SourceReader,
};

pub mod discovery;
pub mod prefetch;

use discovery::{DependencyEntry, absolutize, normalize_path};

/// Read the stdlib `wado-compiler` was built beside and hand it over, so a dev
/// build serves what is on disk now and the compiler itself reads no file.
/// Called wherever a dev build first reaches the stdlib: every host as it is
/// built, [`crate::Engine::new`], and the `wado` binary before it dispatches.
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
pub fn install_dev_stdlib() {
    use wado_compiler::stdlib::{DEV_STDLIB_ROOT, dev_stdlib_files, install_dev_stdlib};

    let root = Path::new(DEV_STDLIB_ROOT);
    install_dev_stdlib(dev_stdlib_files().into_iter().map(|file| {
        let path = root.join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading the stdlib at {}: {e}", path.display()));
        (file.to_string(), source)
    }));
}

/// A release or `wasm32` build embeds the stdlib, so there is nothing to read.
#[cfg(not(all(debug_assertions, not(target_arch = "wasm32"))))]
pub fn install_dev_stdlib() {}

/// `path`, or `.` where it is empty, as the parent of a bare file name is:
/// `canonicalize` and the manifest walk-up both reject `""`.
fn non_empty(path: PathBuf) -> PathBuf {
    if path.as_os_str().is_empty() {
        return PathBuf::from(".");
    }
    path
}

#[derive(Debug)]
pub struct FilesystemCompilerHost {
    base_path: PathBuf,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
}

impl FilesystemCompilerHost {
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        install_dev_stdlib();
        Self {
            base_path: non_empty(base_path),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A sibling host that loads sources relative to `base_path` but shares
    /// this host's diagnostics buffer, so diagnostics emitted through either
    /// remain visible to `diagnostics()` / `has_errors()`. The Kiln pipeline
    /// uses this to read schemas relative to the manifest root while its
    /// diagnostics still gate the consuming `wado compile` / `wado check`.
    #[must_use]
    pub fn rebased(&self, base_path: PathBuf) -> Self {
        Self {
            base_path: non_empty(base_path),
            diagnostics: Arc::clone(&self.diagnostics),
        }
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// The collected buffer, recovering from a poisoned lock.
    ///
    /// One panic under the compiler pipeline would otherwise leave every
    /// later query panicking on the same lock. The contents are plain data,
    /// so recovery is safe. Matches `DiagnosticCollector` in `lib.rs`.
    fn buffer(&self) -> std::sync::MutexGuard<'_, Vec<Diagnostic>> {
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.buffer().clone()
    }

    pub fn has_errors(&self) -> bool {
        self.buffer().iter().any(|d| d.severity == Severity::Error)
    }

    /// Append a diagnostic to the collected buffer without emitting it.
    ///
    /// Wrappers call this after performing their own side effects (e.g. stderr
    /// printing) so the buffer remains the single source of truth for
    /// `has_errors` / `diagnostics()`.
    pub fn collect_diagnostic(&self, diagnostic: Diagnostic) {
        self.buffer().push(diagnostic);
    }

    /// [`CompilerHost::package_root`], telling `probed` each file the walk
    /// looks for and whether one is there. The walk starts from the
    /// module's directory as the OS resolves it, `..` after a symlink
    /// included, since that is the directory [`CompilerHost::load_source`]
    /// reads from. A directory that does not exist has no root, and the load
    /// of the module reports it.
    pub fn package_root_probing(
        &self,
        path: &str,
        probed: impl FnMut(&Path, bool),
    ) -> Option<String> {
        let file = self.base_path.join(path);
        let dir = std::fs::canonicalize(file.parent().expect("a module path names a file")).ok()?;
        let root = discovery::nearest_manifest_dir_probing(&dir, probed)?;
        Some(root.display().to_string())
    }
}

/// A file on disk, read a range at a time.
struct FileReader {
    file: Mutex<File>,
    len: u64,
    path: PathBuf,
}

impl FileReader {
    fn open(path: PathBuf) -> Result<Self, SourceError> {
        let file = File::open(&path).map_err(|e| io_error(&path, &e))?;
        let len = file.metadata().map_err(|e| io_error(&path, &e))?.len();
        Ok(Self {
            file: Mutex::new(file),
            len,
            path,
        })
    }
}

impl SourceReader for FileReader {
    fn byte_len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, start: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.seek(SeekFrom::Start(start))
            .and_then(|_| fill(&mut *file, buf))
            .map_err(|e| io_error(&self.path, &e))
    }
}

/// Read into `buf` until it is full or `from` ends, answering how much was read.
fn fill(from: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        match from.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(read)
}

/// The first `prefix` bytes of the file at `path`, or all of it at `None`. A
/// file shorter than `prefix` answers what it holds.
///
/// # Errors
///
/// What opening or reading the file fails with.
pub fn read_prefix(path: &Path, prefix: Option<u64>) -> std::io::Result<Vec<u8>> {
    let Some(n) = prefix else {
        return std::fs::read(path);
    };
    let mut bytes = Vec::new();
    File::open(path)?.take(n).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether `e` says the file is not there. A path through a file, such as
/// `wado.toml` under a path dependency naming a file, is as absent as a
/// missing one.
#[must_use]
pub fn is_absent(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

fn io_error(path: &Path, e: &std::io::Error) -> SourceError {
    let path = path.display().to_string();
    if is_absent(e) {
        return SourceError::NotFound { path };
    }
    SourceError::IoError {
        path,
        message: e.to_string(),
    }
}

impl CompilerHost for FilesystemCompilerHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.load_source_prefix(path, None).await
    }

    /// No pin applies: only a remote file carries one.
    async fn open_source(
        &self,
        path: &str,
        _: Option<Integrity>,
    ) -> Result<Arc<dyn SourceReader>, SourceError> {
        Ok(Arc::new(FileReader::open(self.base_path.join(path))?))
    }

    async fn load_source_prefix(
        &self,
        path: &str,
        extent: Option<u64>,
    ) -> Result<Vec<u8>, SourceError> {
        let full_path = self.base_path.join(path);
        read_prefix(&full_path, extent).map_err(|e| io_error(&full_path, &e))
    }

    async fn source_exists(&self, path: &str) -> bool {
        self.base_path.join(path).is_file()
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.collect_diagnostic(diagnostic);
    }

    /// An invalid manifest yields an empty index that says why, so the editor
    /// keeps answering and each dependency import reports the manifest.
    fn dependency_index(&self) -> DependencyIndex {
        match discovery::nearest_manifest_dir(&self.base_path) {
            Some(dir) => manifest_index(&dir, &self.base_path),
            None => DependencyIndex::default(),
        }
    }

    fn package_index(&self, root: &str) -> DependencyIndex {
        manifest_index(Path::new(root), &self.base_path)
    }

    fn serves_core(&self) -> bool {
        is_in_dev_stdlib_core(&self.base_path)
    }

    fn package_root(&self, path: &str) -> Option<String> {
        self.package_root_probing(path, |_, _| {})
    }

    /// The language service recompiles on every edit and caches no outcome.
    fn stdlib_module_read(&self, _import_path: &str) {}
}

/// Whether `dir` lies in the `core` tree of the stdlib this dev build reads,
/// as a stdlib test's directory does.
#[cfg(all(debug_assertions, not(target_arch = "wasm32")))]
fn is_in_dev_stdlib_core(dir: &Path) -> bool {
    use wado_compiler::stdlib::DEV_STDLIB_ROOT;

    let core = Path::new(DEV_STDLIB_ROOT).join("core");
    match (std::fs::canonicalize(dir), std::fs::canonicalize(core)) {
        (Ok(dir), Ok(core)) => dir.starts_with(core),
        _ => false,
    }
}

/// A release or `wasm32` build embeds the stdlib, so it runs none of its tests.
#[cfg(not(all(debug_assertions, not(target_arch = "wasm32"))))]
fn is_in_dev_stdlib_core(_dir: &Path) -> bool {
    false
}

/// Build the compiler's dependency index from a manifest's `[dependencies]`,
/// and each source dependency's own, at any depth.
///
/// Source entries are re-expressed relative to `base` — the base `load_source`
/// joins against — so `use { … } from "<name>"` resolves to them.
#[must_use]
pub fn dependency_index_from(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
) -> DependencyIndex {
    let (mut index, entries) = package_index(manifest, manifest_dir, base);
    index_packages(&mut index, entries, base);
    index
}

/// The key `index.resolved` names the source dependency whose entry module is
/// `entry` by: its path relative to `base`, the base `load_source` joins against.
/// Both are taken as the OS reaches them, since that is how `load_source` reads
/// the key back: a `..` climbs a symlink's target, not its spelling. Every
/// index then names one package by one key, however it was reached.
#[must_use]
pub fn dependency_key(entry: &Path, base: &Path) -> String {
    relative_path(&physical(base), &physical(entry))
}

/// `path` as the OS reaches it: its deepest existing ancestor with symlinks
/// resolved, and the rest of it appended. A path nothing of which resolves (as
/// on a host with no filesystem) is taken lexically.
fn physical(path: &Path) -> PathBuf {
    let path = absolutize(path);
    for ancestor in path.ancestors() {
        if let Ok(real) = std::fs::canonicalize(ancestor) {
            let rest = path
                .strip_prefix(ancestor)
                .expect("an ancestor prefixes its path");
            return normalize_path(&real.join(rest));
        }
    }
    normalize_path(&path)
}

/// Add to `index.packages` the package of each source dependency entry module
/// in `entries`, and of each source dependency those reach, at any depth.
pub fn index_packages(index: &mut DependencyIndex, entries: Vec<PathBuf>, base: &Path) {
    let mut walk = PackageWalk::new(entries, base);
    while let Some(package) = walk.next(index) {
        walk.index(index, package);
    }
}

/// The walk over the source dependencies an index reaches, at any depth, each
/// package once by its key. A driver may act on a package [`Self::next`] hands
/// out before [`Self::index`] lists what it depends on.
pub struct PackageWalk {
    pending: VecDeque<PathBuf>,
    base: PathBuf,
}

/// A package [`PackageWalk::next`] hands out: its key, root and manifest.
pub struct WalkedPackage {
    pub lib: String,
    pub root: PathBuf,
    pub manifest: wado_manifest::Manifest,
}

impl PackageWalk {
    /// A walk from the dependency entry modules `entries`, keyed from `base`.
    #[must_use]
    pub fn new(entries: Vec<PathBuf>, base: &Path) -> Self {
        Self {
            pending: entries.into(),
            base: base.to_path_buf(),
        }
    }

    /// The next package `index` does not hold. A single-file dependency, which
    /// has no manifest, is recorded on the way with nothing to index.
    pub fn next(&mut self, index: &mut DependencyIndex) -> Option<WalkedPackage> {
        while let Some(entry) = self.pending.pop_front() {
            let lib = dependency_key(&entry, &self.base);
            if index.packages.contains_key(&lib) {
                continue;
            }
            match discovery::lib_package(&entry) {
                Some((root, manifest)) => {
                    return Some(WalkedPackage {
                        lib,
                        root,
                        manifest,
                    });
                }
                None => {
                    index.packages.insert(lib, DependencyIndex::default());
                }
            }
        }
        None
    }

    /// Index `package` into `index`, and queue the packages it depends on.
    pub fn index<'a>(
        &mut self,
        index: &'a mut DependencyIndex,
        package: WalkedPackage,
    ) -> &'a mut DependencyIndex {
        let (own, reached) = package_index(&package.manifest, &package.root, &self.base);
        self.pending.extend(reached);
        index.packages.entry(package.lib).or_insert(own)
    }
}

/// One package's index, without `packages`, and the absolute entry module of
/// each of its source dependencies, keyed from `base`.
#[must_use]
pub fn package_index(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
) -> (DependencyIndex, Vec<PathBuf>) {
    let root = normalize_path(&absolutize(manifest_dir));
    let exports = manifest.package.iter().flat_map(|p| &p.exports);
    let mut index = DependencyIndex {
        manifest: Some(DependencyManifest {
            path: manifest_path(manifest_dir),
            error: None,
        }),
        exports: exports
            .map(|entry| (entry.clone(), dependency_key(&root.join(entry), base)))
            .collect(),
        ..DependencyIndex::default()
    };
    let mut entries = Vec::new();
    for (name, entry) in discovery::resolve_all(manifest, manifest_dir) {
        match entry {
            Ok(DependencyEntry::Source(path)) => {
                index.resolved.insert(name, dependency_key(&path, base));
                entries.push(path);
            }
            Ok(DependencyEntry::Component(path)) => {
                index.components.insert(name, path.display().to_string());
            }
            Err(reason) => {
                index.unresolved.insert(name, reason);
            }
        }
    }
    (index, entries)
}

/// The index of the package whose `wado.toml` is in `dir`, keyed from `base`.
fn manifest_index(dir: &Path, base: &Path) -> DependencyIndex {
    match read_manifest(dir) {
        Ok(manifest) => dependency_index_from(&manifest, dir, base),
        Err(error) => unreadable_manifest(dir, error),
    }
}

/// The index of a package whose `wado.toml` in `dir` cannot be read: empty,
/// saying why, so each import that needed an entry reports it.
fn unreadable_manifest(dir: &Path, error: String) -> DependencyIndex {
    DependencyIndex {
        manifest: Some(DependencyManifest {
            path: manifest_path(dir),
            error: Some(error),
        }),
        ..DependencyIndex::default()
    }
}

/// The `wado.toml` in `dir`, parsed, or why it cannot be.
fn read_manifest(dir: &Path) -> Result<wado_manifest::Manifest, String> {
    let text = std::fs::read_to_string(dir.join(wado_manifest::MANIFEST_FILENAME))
        .map_err(|e| e.to_string())?;
    discovery::resolve_member_manifest(dir, &text).map_err(|e| e.to_string())
}

/// The absolute path of the `wado.toml` in `dir`, as diagnostics name it.
fn manifest_path(dir: &Path) -> String {
    normalize_path(&absolutize(dir))
        .join(wado_manifest::MANIFEST_FILENAME)
        .display()
        .to_string()
}

/// Lexical relative path from directory `from_dir` to file `to_file`. Both
/// must be absolute; symlinks are not resolved.
fn relative_path(from_dir: &Path, to_file: &Path) -> String {
    let from = normalized_components(from_dir);
    let to = normalized_components(to_file);
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - common];
    parts.extend(to[common..].iter().cloned());
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

fn normalized_components(p: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir | Component::Prefix(_) => {}
            Component::RootDir => out.push(String::new()),
            Component::ParentDir => {
                if matches!(out.last().map(String::as_str), None | Some("..")) {
                    out.push("..".to_string());
                } else {
                    out.pop();
                }
            }
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use crate::uri::Uri;

    #[test]
    fn an_empty_base_path_is_the_current_directory() {
        // `Path::new("a.wado").parent()` is `""`, which `canonicalize` and the
        // manifest walk-up both reject.
        let host = FilesystemCompilerHost::new(PathBuf::new());
        assert_eq!(host.base_path(), Path::new("."));
        assert_eq!(host.rebased(PathBuf::new()).base_path(), Path::new("."));
    }

    #[test]
    fn source_exists_answers_without_reading() {
        // Called per recorded kiln output on every snapshot build.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.wado"), "fn f() {}").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let host = FilesystemCompilerHost::new(tmp.path().to_path_buf());

        futures::executor::block_on(async {
            assert!(host.source_exists("a.wado").await);
            assert!(!host.source_exists("missing.wado").await);
            assert!(!host.source_exists("sub").await);
        });
    }

    // The editor keeps answering on an invalid manifest, so the index carries
    // the reason for the loader to report at each dependency import.
    #[test]
    fn an_invalid_manifest_is_carried_by_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("wado.toml"),
            "[dependencies]\n\"lib:x\" = \"1.0.0\"\n",
        )
        .unwrap();
        let host = FilesystemCompilerHost::new(tmp.path().join("src"));

        let index = host.dependency_index();
        let manifest = index.manifest.expect("the manifest is named");
        assert_eq!(manifest.path, manifest_path(tmp.path()));
        assert!(manifest.error.is_some());
        assert!(index.resolved.is_empty());

        let uri = Uri::from_file_path(&tmp.path().join("src/main.wado"))
            .as_str()
            .to_string();
        let mut engine = Engine::new();
        engine.open_document(&uri, "use { x } from \"lib:x\";\n".to_string());
        let diagnostics = futures::executor::block_on(engine.diagnostics(&uri, &host));
        let reason = format!(
            "cannot resolve dependency 'lib:x': {} is invalid: ",
            manifest_path(tmp.path())
        );
        assert!(
            diagnostics.iter().any(|d| d.message.starts_with(&reason)),
            "{diagnostics:#?}"
        );
    }
}
