//! Filesystem-based `CompilerHost` for embedders that need silent diagnostic
//! collection plus relative-path source loading.
//!
//! This is the default host used by the LSP server. Consumers that additionally
//! want to decorate output (timestamps, log-level filtering, stderr printing)
//! wrap this host. [`discovery`] holds the filesystem reads that answer what
//! this host's [`CompilerHost::dependency_index`] reports.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use wado_compiler::{CompilerHost, DependencyIndex, Diagnostic, Severity, SourceError};

pub mod discovery;

use discovery::{DependencyEntry, absolutize};

#[derive(Debug)]
pub struct FilesystemCompilerHost {
    base_path: PathBuf,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
}

impl FilesystemCompilerHost {
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
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
            base_path,
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
}

impl CompilerHost for FilesystemCompilerHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        let full_path = self.base_path.join(path);
        std::fs::read(&full_path).map_err(|e| SourceError::IoError {
            path: full_path.display().to_string(),
            message: e.to_string(),
        })
    }

    async fn source_exists(&self, path: &str) -> bool {
        self.base_path.join(path).is_file()
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.collect_diagnostic(diagnostic);
    }

    fn dependency_index(&self) -> DependencyIndex {
        let Some((manifest, root)) = nearest_manifest(&self.base_path) else {
            return DependencyIndex::default();
        };
        dependency_index_from(&manifest, &root, &self.base_path)
    }
}

/// Build the compiler's dependency index from a manifest's `[dependencies]`.
///
/// The bridge between what [`discovery::resolve_all`] found on
/// disk and what the loader consults: source entries are re-expressed relative
/// to `base` — the same base `load_source` joins against — so `use { … } from
/// "<name>"` resolves to them, prebuilt components keep their absolute cache
/// path, and an unplaceable dependency carries its reason to the `use` site
/// instead of a generic "invalid module path". `manifest_dir` contains the
/// manifest (path deps and the lock resolve against it).
#[must_use]
pub fn dependency_index_from(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
) -> DependencyIndex {
    let mut index = DependencyIndex::default();
    let base_abs = absolutize(base);
    for (name, entry) in discovery::resolve_all(manifest, manifest_dir) {
        match entry {
            Ok(DependencyEntry::Source(path)) => {
                index
                    .resolved
                    .insert(name, relative_path(&base_abs, &absolutize(&path)));
            }
            Ok(DependencyEntry::Component(path)) => {
                index.components.insert(name, path.display().to_string());
            }
            Err(reason) => {
                index.unresolved.insert(name, reason);
            }
        }
    }
    index
}

/// The nearest `wado.toml` at or above `start`, parsed, with its directory.
fn nearest_manifest(start: &Path) -> Option<(wado_manifest::Manifest, PathBuf)> {
    let dir = discovery::nearest_manifest_dir(start)?;
    let text = std::fs::read_to_string(dir.join(wado_manifest::MANIFEST_FILENAME)).ok()?;
    let manifest = discovery::resolve_member_manifest(&dir, &text).ok()?;
    Some((manifest, dir))
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

    #[test]
    fn source_exists_answers_without_reading() {
        // The kiln presence check calls this per recorded output on every
        // snapshot; a `load_source` in its place would read each file in full
        // only to drop the bytes.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.wado"), "fn f() {}").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let host = FilesystemCompilerHost::new(tmp.path().to_path_buf());

        futures::executor::block_on(async {
            assert!(host.source_exists("a.wado").await);
            assert!(!host.source_exists("missing.wado").await);
            // A directory is not loadable, so it must not read as present.
            assert!(!host.source_exists("sub").await);
        });
    }
}
