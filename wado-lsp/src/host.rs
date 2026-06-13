//! Filesystem-based `CompilerHost` for embedders that need silent diagnostic
//! collection plus relative-path source loading.
//!
//! This is the default host used by the LSP server. Consumers that additionally
//! want to decorate output (timestamps, log-level filtering, stderr printing)
//! wrap this host.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use wado_compiler::{CompilerHost, Diagnostic, Severity, SourceError};
use wado_manifest::DependencySource;

#[derive(Debug)]
pub struct FilesystemCompilerHost {
    base_path: PathBuf,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl FilesystemCompilerHost {
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
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

    fn dependency_index(&self) -> HashMap<String, String> {
        dependency_index(&self.base_path)
    }
}

/// Build the bare-name → entry-path dependency index from the nearest
/// `wado.toml`'s `[dependencies]`. Each path dependency's entry module (its
/// `[package].lib`, or the file itself for a single-`.wado` path dependency)
/// is recorded relative to `base` — the same base `load_source` joins against
/// — so `use { … } from "<name>"` resolves to it.
///
/// Only `path` dependencies are supported for now; registry/git are skipped.
fn dependency_index(base: &Path) -> HashMap<String, String> {
    let mut index = HashMap::new();
    let Some((manifest, root)) = nearest_manifest(base) else {
        return index;
    };
    let base_abs = absolutize(base);
    for (name, dep) in &manifest.dependencies {
        let DependencySource::Path { path, .. } = &dep.source else {
            continue;
        };
        let Some(entry) = dependency_entry_path(&root.join(path)) else {
            continue;
        };
        index.insert(name.clone(), relative_path(&base_abs, &absolutize(&entry)));
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
/// points at a `.wado` file, otherwise the directory's `[package].lib`.
fn dependency_entry_path(dep_path: &Path) -> Option<PathBuf> {
    if dep_path.extension().is_some_and(|e| e == "wado") {
        return Some(dep_path.to_path_buf());
    }
    let text = std::fs::read_to_string(dep_path.join("wado.toml")).ok()?;
    let manifest: wado_manifest::Manifest = text.parse().ok()?;
    Some(dep_path.join(manifest.package?.lib?))
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
