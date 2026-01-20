//! Filesystem-based compiler host for CLI usage
//!
//! This module provides `FilesystemCompilerHost` which implements
//! `wado_compiler::CompilerHost` using filesystem I/O.

use std::path::PathBuf;
use std::sync::Mutex;

use wado_compiler::{CompilerHost, Diagnostic, Severity, SourceError};

/// Filesystem-based compiler host
///
/// This is the standard host used by the CLI. It loads sources from the filesystem
/// and prints diagnostics to stderr.
#[derive(Debug)]
pub struct FilesystemCompilerHost {
    /// Base path for resolving relative imports
    base_path: PathBuf,
    /// Collected diagnostics (for programmatic access)
    diagnostics: Mutex<Vec<Diagnostic>>,
    /// Whether to print diagnostics to stderr
    print_diagnostics: bool,
}

impl FilesystemCompilerHost {
    /// Create a new filesystem host with the given base path
    #[must_use] 
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
            print_diagnostics: true,
        }
    }

    /// Create a host that collects diagnostics without printing
    #[must_use] 
    pub fn silent(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
            print_diagnostics: false,
        }
    }

    /// Get all collected diagnostics
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }

    /// Check if any errors were reported
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Get the base path
    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }
}

impl CompilerHost for FilesystemCompilerHost {
    async fn load_source(&self, path: &str) -> Result<String, SourceError> {
        let full_path = self.base_path.join(path);
        std::fs::read_to_string(&full_path).map_err(|e| SourceError::IoError {
            path: full_path.display().to_string(),
            message: e.to_string(),
        })
    }

    async fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        if self.print_diagnostics {
            eprintln!("{diagnostic}");
        }
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}
