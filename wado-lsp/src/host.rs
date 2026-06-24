//! Filesystem-based `CompilerHost` for embedders that need silent diagnostic
//! collection plus relative-path source loading.
//!
//! This is the default host used by the LSP server. Consumers that additionally
//! want to decorate output (timestamps, log-level filtering, stderr printing)
//! wrap this host.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use wado_compiler::{CompilerHost, DependencyIndex, Diagnostic, Severity, SourceError};
use wado_manifest::DependencySource;

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

    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Append a diagnostic to the collected buffer without emitting it.
    ///
    /// Wrappers call this after performing their own side effects (e.g. stderr
    /// printing) so the buffer remains the single source of truth for
    /// `has_errors` / `diagnostics()`.
    pub fn collect_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
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

/// Build the bare-name → entry-path dependency index from a manifest's
/// `[dependencies]`. Each path dependency's entry module (its
/// `[package].lib`, or the file itself for a single-`.wado` path dependency)
/// is recorded relative to `base` — the same base `load_source` joins against
/// — so `use { … } from "<name>"` resolves to it. `manifest_dir` is the
/// directory containing the manifest (path deps resolve against it).
///
/// Only `path` dependencies are supported for now; registry/git are skipped.
#[must_use]
pub fn dependency_index_from(
    manifest: &wado_manifest::Manifest,
    manifest_dir: &Path,
    base: &Path,
) -> DependencyIndex {
    let mut index = DependencyIndex::default();
    let base_abs = absolutize(base);
    for (name, dep) in &manifest.dependencies {
        let DependencySource::Path { path, .. } = &dep.source else {
            continue;
        };
        match dependency_entry_path(&manifest_dir.join(path)) {
            Ok(entry) => {
                index
                    .resolved
                    .insert(name.clone(), relative_path(&base_abs, &absolutize(&entry)));
            }
            Err(reason) => {
                index.unresolved.insert(name.clone(), reason);
            }
        }
    }
    index
}

/// Walk up from `start` to find the nearest `wado.toml`, returning the parsed
/// manifest and its directory.
fn nearest_manifest(start: &Path) -> Option<(wado_manifest::Manifest, PathBuf)> {
    let mut dir = absolutize(start);
    loop {
        let candidate = dir.join("wado.toml");
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let manifest: wado_manifest::Manifest = text.parse().ok()?;
            return Some((manifest, dir));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The entry module file of a path dependency: the file itself when the path
/// points at a `.wado` file, otherwise the directory's `[package].lib`. The
/// `Err` describes why a declared dependency has no usable entry.
fn dependency_entry_path(dep_path: &Path) -> Result<PathBuf, String> {
    if dep_path.extension().is_some_and(|e| e == "wado") {
        return Ok(dep_path.to_path_buf());
    }
    let manifest_path = dep_path.join("wado.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
    let manifest: wado_manifest::Manifest = text
        .parse()
        .map_err(|e| format!("invalid {}: {e}", manifest_path.display()))?;
    let lib = manifest.package.and_then(|p| p.lib).ok_or_else(|| {
        format!(
            "{} declares no [package].lib entry",
            manifest_path.display()
        )
    })?;
    Ok(dep_path.join(lib))
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
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
