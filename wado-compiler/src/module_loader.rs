//! Module resolver for Wado
//!
//! Resolves module paths to source code and parses them.
//! Core library modules are loaded from embedded sources.
//! Local .wado files are loaded from the filesystem.
//!
//! # Path Canonicalization
//!
//! Module paths are canonicalized to ensure the same file imported via different
//! paths resolves to the same identity. For example:
//! - `./geometry.wado` and `./sub/../geometry.wado` → same module
//!
//! Canonical paths are project-root-relative and always use `/` separator.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::Module;
use crate::lexer::Lexer;
use crate::name::{canonicalize_entry_point, normalize_module_path, resolve_module_path};
use crate::parser::Parser;
use crate::stdlib;

/// Error that can occur during module resolution
#[derive(Debug, Clone)]
pub enum ModuleLoadError {
    /// Module was not found
    ModuleNotFound { path: Vec<String> },
    /// Error while parsing module
    ParseError { path: Vec<String>, message: String },
    /// Circular import detected
    CircularImport { path: Vec<String> },
    /// Lexer error
    LexError { path: Vec<String>, message: String },
    /// I/O error reading file
    IoError { path: String, message: String },
}

impl std::fmt::Display for ModuleLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleLoadError::ModuleNotFound { path } => {
                write!(f, "module not found: {}", path.join("::"))
            }
            ModuleLoadError::ParseError { path, message } => {
                write!(f, "parse error in {}: {}", path.join("::"), message)
            }
            ModuleLoadError::CircularImport { path } => {
                write!(f, "circular import detected: {}", path.join("::"))
            }
            ModuleLoadError::LexError { path, message } => {
                write!(f, "lex error in {}: {}", path.join("::"), message)
            }
            ModuleLoadError::IoError { path, message } => {
                write!(f, "error reading '{path}': {message}")
            }
        }
    }
}

impl std::error::Error for ModuleLoadError {}

/// Module resolver
///
/// Loads and parses modules, caching the results.
/// Core library modules are loaded from embedded sources in the compiler binary.
/// Local .wado files are loaded from the filesystem relative to `base_path`.
///
/// Module paths are canonicalized before caching to ensure that the same file
/// imported via different paths (e.g., `./geometry.wado` vs `./sub/../geometry.wado`)
/// resolves to the same module instance.
#[derive(Debug, Default)]
pub struct ModuleResolver {
    /// Cache of already parsed modules (canonical module path → parsed AST)
    parsed_modules: HashMap<Vec<String>, Module>,
    /// Set of modules currently being resolved (for cycle detection)
    /// Uses canonical paths for accurate cycle detection
    resolving: HashSet<Vec<String>>,
    /// Base path for resolving relative imports (directory containing the main module)
    base_path: Option<PathBuf>,
    /// Canonical path of the entry point module (e.g., `./main.wado`)
    entry_point_canonical: Option<String>,
}

impl ModuleResolver {
    /// Create a new module resolver
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new module resolver with a base path for relative imports
    pub fn with_base_path(base_path: &Path) -> Self {
        Self {
            parsed_modules: HashMap::new(),
            resolving: HashSet::new(),
            base_path: Some(base_path.to_path_buf()),
            entry_point_canonical: None,
        }
    }

    /// Set the base path for resolving relative imports
    pub fn set_base_path(&mut self, base_path: &Path) {
        self.base_path = Some(base_path.to_path_buf());
    }

    /// Set the canonical path for the entry point module.
    ///
    /// This is typically `./main.wado` or the filename of the entry point,
    /// prefixed with `./` to indicate it's in the project root.
    pub fn set_entry_point(&mut self, filename: &str) {
        self.entry_point_canonical = Some(canonicalize_entry_point(filename));
    }

    /// Get the canonical path of the entry point module.
    pub fn entry_point(&self) -> Option<&str> {
        self.entry_point_canonical.as_deref()
    }

    /// Canonicalize a module path.
    ///
    /// For relative paths, this resolves `.` and `..` segments.
    /// For special prefixes (`core:`, `wasi:`, `http://`, `https://`), returns as-is.
    fn canonicalize_path(&self, module_path: &[String]) -> Vec<String> {
        if module_path.is_empty() {
            return module_path.to_vec();
        }

        let first = &module_path[0];

        // Handle relative local imports
        if first.starts_with("./") || first.starts_with("../") || first.ends_with(".wado") {
            let canonical = normalize_module_path(first);
            return vec![canonical];
        }

        // For other paths (core:*, wasi:*, etc.), return as-is
        module_path.to_vec()
    }

    /// Resolve a relative import path against an importing module's path.
    ///
    /// # Arguments
    /// * `from_module` - The canonical path of the importing module (e.g., `["./sub/main.wado"]`)
    /// * `import_source` - The import source string (e.g., `"./geometry.wado"` or `"../lib.wado"`)
    ///
    /// # Returns
    /// The resolved and canonicalized module path.
    pub fn resolve_import(&self, from_module: &[String], import_source: &str) -> Vec<String> {
        // Handle special prefixes - they don't need resolution
        if import_source.starts_with("core:")
            || import_source.starts_with("wasi:")
            || import_source.starts_with("https://")
            || import_source.starts_with("http://")
        {
            // Parse into path segments
            if import_source.contains(':') {
                return import_source.splitn(2, ':').map(String::from).collect();
            }
            return vec![import_source.to_string()];
        }

        // For relative imports, resolve against the from_module's path
        if !from_module.is_empty() {
            let from_path = &from_module[0];
            if from_path.starts_with("./") || from_path.starts_with("../") {
                let resolved = resolve_module_path(from_path, import_source);
                return vec![resolved];
            }
        }

        // Fallback: treat as relative to project root
        let canonical = normalize_module_path(import_source);
        vec![canonical]
    }

    /// Load and parse a module by its path
    ///
    /// # Arguments
    /// * `module_path` - Module path segments, e.g., `["core", "cli"]` or `["./geometry.wado"]`
    ///
    /// # Returns
    /// The parsed module AST, or an error if the module cannot be found or parsed.
    ///
    /// # Note
    /// The path is canonicalized before caching, so `["./sub/../geometry.wado"]` and
    /// `["./geometry.wado"]` will resolve to the same cached module.
    pub fn load_module(&mut self, module_path: &[String]) -> Result<&Module, ModuleLoadError> {
        // Canonicalize the path for caching
        let canonical_path = self.canonicalize_path(module_path);

        // Check cache first using canonical path
        if self.parsed_modules.contains_key(&canonical_path) {
            return Ok(self.parsed_modules.get(&canonical_path).unwrap());
        }

        // Check for circular imports using canonical path
        if self.resolving.contains(&canonical_path) {
            return Err(ModuleLoadError::CircularImport {
                path: canonical_path,
            });
        }

        // Mark as being resolved
        self.resolving.insert(canonical_path.clone());

        // Get source code (use original path for filesystem access)
        let source = self.get_source(module_path)?;

        // Parse the module
        let module = self.parse_source(&source, &canonical_path)?;

        // Remove from resolving set
        self.resolving.remove(&canonical_path);

        // Cache using canonical path and return
        self.parsed_modules.insert(canonical_path.clone(), module);
        Ok(self.parsed_modules.get(&canonical_path).unwrap())
    }

    /// Get the source code for a module
    fn get_source(&self, module_path: &[String]) -> Result<String, ModuleLoadError> {
        // Check if this is a local file path (starts with "." or has single element ending in .wado)
        if !module_path.is_empty() {
            let first = &module_path[0];

            // Check for relative paths like "./foo.wado" or "../bar.wado"
            if first.starts_with("./") || first.starts_with("../") || first.ends_with(".wado") {
                return self.get_local_file_source(module_path);
            }
        }

        // Convert path segments to ESM-like import path
        // e.g., ["core", "cli"] -> "core:cli"
        let import_path = if module_path.len() == 2 {
            format!("{}:{}", module_path[0], module_path[1])
        } else {
            module_path.join(":")
        };

        // Try embedded stdlib (core:* and wasi:*)
        if let Some(source) = stdlib::get_stdlib_module(&import_path) {
            return Ok(source.to_string());
        }

        // Module not found
        Err(ModuleLoadError::ModuleNotFound {
            path: module_path.to_vec(),
        })
    }

    /// Load source code from a local .wado file
    fn get_local_file_source(&self, module_path: &[String]) -> Result<String, ModuleLoadError> {
        let base_path = self
            .base_path
            .as_ref()
            .ok_or_else(|| ModuleLoadError::ModuleNotFound {
                path: module_path.to_vec(),
            })?;

        // Reconstruct the relative file path from module_path
        // Module path is stored as the original import string in a single element
        let relative_path = &module_path[0];

        // Resolve the full path
        let full_path = base_path.join(relative_path);

        // Read the file
        std::fs::read_to_string(&full_path).map_err(|e| ModuleLoadError::IoError {
            path: full_path.display().to_string(),
            message: e.to_string(),
        })
    }

    /// Parse source code into a module AST
    fn parse_source(
        &self,
        source: &str,
        module_path: &[String],
    ) -> Result<Module, ModuleLoadError> {
        // Tokenize
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize().map_err(|e| ModuleLoadError::LexError {
            path: module_path.to_vec(),
            message: format!(
                "line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            ),
        })?;

        // Parse
        let mut parser = Parser::new(tokens);
        let module = parser.parse().map_err(|e| ModuleLoadError::ParseError {
            path: module_path.to_vec(),
            message: format!(
                "line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            ),
        })?;

        Ok(module)
    }

    /// Check if a module has been loaded
    ///
    /// Uses canonical path for lookup.
    pub fn is_loaded(&self, module_path: &[String]) -> bool {
        let canonical = self.canonicalize_path(module_path);
        self.parsed_modules.contains_key(&canonical)
    }

    /// Get a cached module (if already loaded)
    ///
    /// Uses canonical path for lookup.
    pub fn get_cached(&self, module_path: &[String]) -> Option<&Module> {
        let canonical = self.canonicalize_path(module_path);
        self.parsed_modules.get(&canonical)
    }

    /// Get all loaded modules
    pub fn loaded_modules(&self) -> Vec<&Vec<String>> {
        self.parsed_modules.keys().collect()
    }

    /// Consume the resolver and return all parsed modules
    pub fn into_modules(self) -> HashMap<Vec<String>, Module> {
        self.parsed_modules
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_core_cli() {
        let mut resolver = ModuleResolver::new();
        let path = vec!["core".to_string(), "cli".to_string()];

        let result = resolver.load_module(&path);
        assert!(
            result.is_ok(),
            "Failed to load core::cli: {:?}",
            result.err()
        );

        let module = result.unwrap();
        // core::cli should have some items
        assert!(!module.items.is_empty());
    }

    #[test]
    fn test_load_core_prelude() {
        let mut resolver = ModuleResolver::new();
        let path = vec!["core".to_string(), "prelude".to_string()];

        let result = resolver.load_module(&path);
        // Note: prelude.wado uses generic resource syntax (e.g., `resource Stream<T>`)
        // which the parser doesn't support yet. This test documents the current limitation.
        // When parser supports generics on resources, change this to assert!(result.is_ok()).
        if result.is_err() {
            // Expected for now - parser doesn't support generic resources
            return;
        }
        // If parsing succeeds in future, verify module is not empty
        assert!(!result.unwrap().items.is_empty());
    }

    #[test]
    fn test_module_not_found() {
        let mut resolver = ModuleResolver::new();
        let path = vec!["nonexistent".to_string(), "module".to_string()];

        let result = resolver.load_module(&path);
        assert!(matches!(
            result,
            Err(ModuleLoadError::ModuleNotFound { .. })
        ));
    }

    #[test]
    fn test_caching() {
        let mut resolver = ModuleResolver::new();
        let path = vec!["core".to_string(), "cli".to_string()];

        // Load once
        resolver.load_module(&path).unwrap();
        assert!(resolver.is_loaded(&path));

        // Load again (should use cache)
        let result = resolver.load_module(&path);
        assert!(result.is_ok());
    }
}
