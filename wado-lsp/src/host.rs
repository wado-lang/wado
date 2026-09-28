//! Filesystem-based `CompilerHost` for embedders that need silent diagnostic
//! collection plus relative-path source loading.
//!
//! This is the default host used by the LSP server. Consumers that additionally
//! want to decorate output (timestamps, log-level filtering, stderr printing)
//! wrap this host. [`discovery`] holds the filesystem reads that answer what
//! this host's [`CompilerHost::dependency_index`] reports.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use wado_compiler::{
    CompilerHost, DependencyIndex, DependencyManifest, Diagnostic, Severity, SourceError,
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

    /// An invalid manifest yields an empty index that says why, so the editor
    /// keeps answering and each dependency import reports the manifest.
    fn dependency_index(&self) -> DependencyIndex {
        let Some(dir) = discovery::nearest_manifest_dir(&self.base_path) else {
            return DependencyIndex::default();
        };
        match read_manifest(&dir) {
            Ok(manifest) => dependency_index_from(&manifest, &dir, &self.base_path),
            Err(error) => DependencyIndex {
                manifest: Some(DependencyManifest {
                    path: manifest_path(&dir),
                    error: Some(error),
                }),
                ..DependencyIndex::default()
            },
        }
    }
}

/// Build the compiler's dependency index from a manifest's `[dependencies]`.
///
/// Source entries are re-expressed relative to `base` — the base `load_source`
/// joins against — so `use { … } from "<name>"` resolves to them.
#[must_use]
pub fn dependency_index_from(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
) -> DependencyIndex {
    let mut index = DependencyIndex {
        manifest: Some(DependencyManifest {
            path: manifest_path(manifest_dir),
            error: None,
        }),
        ..DependencyIndex::default()
    };
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
