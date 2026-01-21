//! Name mangling utilities for Wado compiler
//!
//! This module centralizes all naming/mangling logic for methods, effects, and other symbols.
//!
//! # Naming Conventions
//!
//! ## Method Names
//! - Simple: `{struct_name}::{method_name}` (e.g., `Point::sum`)
//! - Full: `{filename}/{struct_name}::{method_name}` (e.g., `./geometry.wado/Point::sum`)
//! - With trait: `{filename}/{struct_name}^{trait_name}::{method_name}` (e.g., `./geometry.wado/Point^Display::fmt`)
//!
//! ## Effect Operation Names
//! - Qualified: `{effect_name}::{operation_name}` (e.g., `Stdout::write_via_stream`)
//!
//! ## WASI Names
//! - Full: `wasi:{package}/{interface}::{function}` (e.g., `wasi:cli/stdout::write-via-stream`)
//!
//! ## Module-Qualified Names
//! - Function: `{module_path}/{function_name}` (e.g., `./utils.wado/helper`)
//! - Struct: `{module_path}::{struct_name}` (e.g., `./geometry.wado::Point`)
//!
//! # Module Path Canonicalization
//!
//! Module paths are canonicalized using URI path normalization (RFC 3986) to ensure:
//! - Same file imported via different paths resolves to same identity
//! - Always uses `/` separator (platform-agnostic, even on Windows)
//! - Resolves `.` and `..` segments
//!
//! Canonical paths are project-root-relative:
//! - For projects with `wado.toml`: relative to the directory containing `wado.toml`
//! - For standalone scripts: relative to the entry point's directory

use fluent_uri::UriRef;
use std::fmt;
use std::hash::{Hash, Hasher};

// =============================================================================
// Module Source Types
// =============================================================================

/// Source location of a module.
///
/// This enum provides a structured representation of module paths,
/// replacing raw `Vec<String>` for better type safety and clearer semantics.
///
/// # Examples
///
/// ```ignore
/// // Core library modules
/// ModuleSource::Core { name: "prelude".to_string() }  // core:prelude
/// ModuleSource::Core { name: "cli".to_string() }      // core:cli
///
/// // WASI modules
/// ModuleSource::Wasi { interface: "cli".to_string() } // wasi:cli
///
/// // Local modules
/// ModuleSource::Local { path: "./geometry.wado".to_string() }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModuleSource {
    /// Core library module (e.g., `core:prelude`, `core:cli`, `core:internal`, `core:builtin`)
    Core {
        /// Module name within core (e.g., "prelude", "cli", "internal", "builtin")
        name: String,
    },
    /// WASI module (e.g., `wasi:cli`, `wasi:io`)
    Wasi {
        /// Interface name (e.g., "cli", "io", "filesystem")
        interface: String,
    },
    /// Local module relative to project root
    Local {
        /// Relative path (e.g., "./geometry.wado", "./utils/helper.wado")
        path: String,
    },
    /// Entry point module (the main file being compiled)
    EntryPoint,
}

impl ModuleSource {
    /// Create a core module source.
    #[must_use]
    pub fn core(name: impl Into<String>) -> Self {
        Self::Core { name: name.into() }
    }

    /// Create a WASI module source.
    #[must_use]
    pub fn wasi(interface: impl Into<String>) -> Self {
        Self::Wasi {
            interface: interface.into(),
        }
    }

    /// Create a local module source.
    #[must_use]
    pub fn local(path: impl Into<String>) -> Self {
        Self::Local { path: path.into() }
    }

    /// Convert from a legacy `Vec<String>` module path.
    ///
    /// This enables gradual migration from the old representation.
    #[must_use]
    pub fn from_path(path: &[String]) -> Self {
        match path {
            [] => Self::EntryPoint,
            [first] if first.starts_with("./") || first.starts_with("../") => Self::Local {
                path: first.clone(),
            },
            [first, rest @ ..] if first == "core" => Self::Core {
                name: rest.join("/"),
            },
            [first, rest @ ..] if first == "wasi" => Self::Wasi {
                interface: rest.join("/"),
            },
            segments => {
                // Treat as local path
                Self::Local {
                    path: segments.join("/"),
                }
            }
        }
    }

    /// Convert to the legacy `Vec<String>` module path representation.
    ///
    /// This enables gradual migration while maintaining compatibility.
    #[must_use]
    pub fn to_path(&self) -> Vec<String> {
        match self {
            Self::Core { name } => vec!["core".to_string(), name.clone()],
            Self::Wasi { interface } => vec!["wasi".to_string(), interface.clone()],
            Self::Local { path } => vec![path.clone()],
            Self::EntryPoint => vec![],
        }
    }

    /// Check if this is a core module.
    #[must_use]
    pub fn is_core(&self) -> bool {
        matches!(self, Self::Core { .. })
    }

    /// Check if this is a WASI module.
    #[must_use]
    pub fn is_wasi(&self) -> bool {
        matches!(self, Self::Wasi { .. })
    }

    /// Check if this is a local module.
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    /// Check if this is the core/internal module.
    #[must_use]
    pub fn is_core_internal(&self) -> bool {
        matches!(self, Self::Core { name } if name == "internal")
    }

    /// Check if this is the core/builtin module.
    #[must_use]
    pub fn is_core_builtin(&self) -> bool {
        matches!(self, Self::Core { name } if name == "builtin")
    }

    /// Check if this is the core/prelude module.
    #[must_use]
    pub fn is_core_prelude(&self) -> bool {
        matches!(self, Self::Core { name } if name == "prelude")
    }
}

impl fmt::Display for ModuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core { name } => write!(f, "core:{name}"),
            Self::Wasi { interface } => write!(f, "wasi:{interface}"),
            Self::Local { path } => write!(f, "{path}"),
            Self::EntryPoint => write!(f, "<entry>"),
        }
    }
}

// =============================================================================
// Function Identifier Types
// =============================================================================

/// A free function name (not a method on a struct).
///
/// Format: `{module_path}/{name}`
///
/// Examples:
/// - `./geometry.wado/helper`
/// - `core/internal/log_stdout`
#[derive(Debug, Clone)]
pub struct FreeFunctionName {
    /// The module path segments (e.g., `[".", "geometry.wado"]`)
    pub module_path: Vec<String>,
    /// The function name (e.g., `helper`)
    pub name: String,
    /// Whether this function is monomorphized (instantiated from a generic)
    pub is_monomorphized: bool,
    /// Base generic name if monomorphized (e.g., "Array" for "Array<i32>`::len`")
    pub base_name: Option<String>,
}

// Manually implement Hash/Eq to only use module_path and name (not metadata)
impl Hash for FreeFunctionName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.module_path.hash(state);
        self.name.hash(state);
    }
}

impl PartialEq for FreeFunctionName {
    fn eq(&self, other: &Self) -> bool {
        self.module_path == other.module_path && self.name == other.name
    }
}

impl Eq for FreeFunctionName {}

impl FreeFunctionName {
    pub fn new(module_path: Vec<String>, name: String) -> Self {
        Self {
            module_path,
            name,
            is_monomorphized: false,
            base_name: None,
        }
    }

    pub fn from_path_and_name(module_path: &[String], name: &str) -> Self {
        Self {
            module_path: module_path.to_vec(),
            name: name.to_string(),
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` from string literal slices.
    /// Convenience method for when you have &[&str] instead of &[String].
    pub fn from_strs(module_path: &[&str], name: &str) -> Self {
        Self {
            module_path: module_path.iter().map(|s| (*s).to_string()).collect(),
            name: name.to_string(),
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` from `ModuleSource` and name.
    pub fn from_module_source(module_source: &ModuleSource, name: &str) -> Self {
        Self {
            module_path: module_source.to_path(),
            name: name.to_string(),
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` with monomorphization metadata.
    pub fn with_monomorph_info(module_path: Vec<String>, name: String, base_name: String) -> Self {
        Self {
            module_path,
            name,
            is_monomorphized: true,
            base_name: Some(base_name),
        }
    }
}

impl fmt::Display for FreeFunctionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.module_path.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}/{}", self.module_path.join("/"), self.name)
        }
    }
}

/// A method name on a struct.
///
/// Format:
/// - Without trait: `{filename}/{struct_name}::{method_name}`
/// - With trait: `{filename}/{struct_name}^{trait_name}::{method_name}`
///
/// Examples:
/// - `./geometry.wado/Point::sum`
/// - `./geometry.wado/Point^Display::fmt`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodName {
    /// The source filename (e.g., `./geometry.wado`)
    pub filename: String,
    /// The struct name (e.g., `Point`)
    pub struct_name: String,
    /// The trait name if this is a trait implementation (e.g., `Display`)
    pub trait_name: Option<String>,
    /// The method name (e.g., `sum`)
    pub method_name: String,
}

impl MethodName {
    pub fn new(
        filename: String,
        struct_name: String,
        trait_name: Option<String>,
        method_name: String,
    ) -> Self {
        Self {
            filename,
            struct_name,
            trait_name,
            method_name,
        }
    }

    /// Create a `MethodName` from `ModuleSource`.
    pub fn from_module_source(
        module_source: &ModuleSource,
        struct_name: &str,
        trait_name: Option<&str>,
        method_name: &str,
    ) -> Self {
        Self {
            filename: module_source.to_path().join("/"),
            struct_name: struct_name.to_string(),
            trait_name: trait_name.map(String::from),
            method_name: method_name.to_string(),
        }
    }

    /// Returns the local part of the method name without the module path.
    /// Format: `Struct^Trait::method` or `Struct::method`
    pub fn local_name(&self) -> String {
        Self::format_local(
            &self.struct_name,
            self.trait_name.as_deref(),
            &self.method_name,
        )
    }

    /// Format a local method name (without module path).
    /// This is the canonical way to build method names like `Struct^Trait::method`.
    pub fn format_local(struct_name: &str, trait_name: Option<&str>, method_name: &str) -> String {
        match trait_name {
            Some(trait_n) => format!("{struct_name}^{trait_n}::{method_name}"),
            None => format!("{struct_name}::{method_name}"),
        }
    }

    /// Format a struct name with type arguments and optional trait.
    /// Format: `Struct<TypeArgs>^Trait` or `Struct<TypeArgs>`
    pub fn format_struct_with_args(
        struct_name: &str,
        type_args: &[String],
        trait_name: Option<&str>,
    ) -> String {
        let struct_part = if type_args.is_empty() {
            struct_name.to_string()
        } else {
            format!("{}<{}>", struct_name, type_args.join(","))
        };
        match trait_name {
            Some(trait_n) => format!("{struct_part}^{trait_n}"),
            None => struct_part,
        }
    }

    /// Join a struct part (which may include ^Trait) with a method part.
    /// This is the final step of method name construction.
    pub fn join_struct_method(struct_part: &str, method_part: &str) -> String {
        format!("{struct_part}::{method_part}")
    }

    /// Format a method name with type arguments.
    /// Format: `method<TypeArgs>` or `method`
    pub fn format_method_with_args(method_name: &str, type_args: &[String]) -> String {
        if type_args.is_empty() {
            method_name.to_string()
        } else {
            format!("{}<{}>", method_name, type_args.join(","))
        }
    }
}

impl fmt::Display for MethodName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.trait_name {
            Some(trait_name) => {
                write!(
                    f,
                    "{}/{}^{}::{}",
                    self.filename, self.struct_name, trait_name, self.method_name
                )
            }
            None => {
                write!(
                    f,
                    "{}/{}::{}",
                    self.filename, self.struct_name, self.method_name
                )
            }
        }
    }
}

/// Extract the local part of a potentially module-qualified name.
///
/// Given a name like `module/path/LocalName`, returns `LocalName`.
/// If there's no module path, returns the original string.
///
/// Examples:
/// - `"./main.wado/Point::sum"` → `"Point::sum"`
/// - `"core/prelude/String::len"` → `"String::len"`
/// - `"Point::sum"` → `"Point::sum"`
pub fn extract_local_name(name: &str) -> &str {
    // Find the last '/' which separates module path from local name
    if let Some(slash_pos) = name.rfind('/') {
        &name[slash_pos + 1..]
    } else {
        name
    }
}

/// Parsed components of a local method name (without module path).
///
/// This is used to extract struct/trait/method info from names like:
/// - `Point::sum` → `struct_name="Point`", `trait_name=None`, `method_name="sum`"
/// - `Point^Display::fmt` → `struct_name="Point`", `trait_name=Some("Display`"), `method_name="fmt`"
#[derive(Debug, Clone)]
pub struct LocalMethodName {
    /// The struct name (e.g., "Point" or "Point<i32>")
    pub struct_name: String,
    /// The trait name if this is a trait method (e.g., "Display")
    pub trait_name: Option<String>,
    /// The method name (e.g., "sum" or "fmt")
    pub method_name: String,
    /// Method-level type args (e.g., ["i64"] for transform<i64>)
    pub method_type_args: Vec<String>,
}

impl LocalMethodName {
    /// Create a new `LocalMethodName` directly from components.
    #[must_use]
    pub fn new(struct_name: String, trait_name: Option<String>, method_name: String) -> Self {
        Self {
            struct_name,
            trait_name,
            method_name,
            method_type_args: vec![],
        }
    }

    /// Create a new `LocalMethodName` with all components including method type args.
    #[must_use]
    pub fn with_method_type_args(
        struct_name: String,
        trait_name: Option<String>,
        method_name: String,
        method_type_args: Vec<String>,
    ) -> Self {
        Self {
            struct_name,
            trait_name,
            method_name,
            method_type_args,
        }
    }

    /// Create a version of this `LocalMethodName` with type args applied.
    ///
    /// `impl_type_args` are applied to the struct name (e.g., "Array" + ["i32"] → "Array<i32>").
    /// `method_type_args` are stored separately (not embedded in `method_name`).
    #[must_use]
    pub fn with_type_args(&self, impl_type_args: &[String], method_type_args: &[String]) -> Self {
        let mangled_struct = if impl_type_args.is_empty() {
            self.struct_name.clone()
        } else {
            format!("{}<{}>", self.struct_name, impl_type_args.join(","))
        };
        Self {
            struct_name: mangled_struct,
            trait_name: self.trait_name.clone(),
            method_name: self.method_name.clone(),
            method_type_args: method_type_args.to_vec(),
        }
    }

    /// Create a version with only struct type args (no method type args).
    /// This is a convenience method for the common case.
    #[must_use]
    pub fn with_struct_type_args(&self, type_args: &[String]) -> Self {
        self.with_type_args(type_args, &[])
    }

    /// Get the full method name including type args (e.g., "transform<i64>")
    #[must_use]
    pub fn full_method_name(&self) -> String {
        if self.method_type_args.is_empty() {
            self.method_name.clone()
        } else {
            format!("{}<{}>", self.method_name, self.method_type_args.join(","))
        }
    }

    /// Parse a local method name string into its components.
    ///
    /// Expected formats:
    /// - `StructName::method`
    /// - `StructName^TraitName::method`
    /// - `StructName<TypeArgs>::method`
    /// - `StructName<TypeArgs>^TraitName::method`
    ///
    /// Returns `None` if the format is invalid (no `::` separator).
    pub fn parse(name: &str) -> Option<Self> {
        let sep_pos = name.find("::")?;
        let prefix = &name[..sep_pos];
        let method_part = &name[sep_pos + 2..];

        // Parse method name and type args (e.g., "transform<i64>" -> "transform", ["i64"])
        let (method_name, method_type_args) = if let Some(angle_pos) = method_part.find('<') {
            let base_name = &method_part[..angle_pos];
            // Extract type args from between < and >
            let type_args_str = method_part
                .strip_prefix(&format!("{base_name}<"))
                .and_then(|s| s.strip_suffix('>'))
                .unwrap_or("");
            let type_args: Vec<String> = type_args_str
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            (base_name, type_args)
        } else {
            (method_part, vec![])
        };

        // Check for trait separator `^` in the prefix
        if let Some(caret_pos) = prefix.find('^') {
            let struct_name = &prefix[..caret_pos];
            let trait_name = &prefix[caret_pos + 1..];
            Some(Self {
                struct_name: struct_name.to_string(),
                trait_name: Some(trait_name.to_string()),
                method_name: method_name.to_string(),
                method_type_args,
            })
        } else {
            Some(Self {
                struct_name: prefix.to_string(),
                trait_name: None,
                method_name: method_name.to_string(),
                method_type_args,
            })
        }
    }

    /// Parse a potentially module-qualified method name.
    ///
    /// This first strips the module path (e.g., `./main.wado/`) and then
    /// parses the local method name.
    ///
    /// Examples:
    /// - `"./main.wado/Point::sum"` → Some(LocalMethodName { `struct_name`: "Point", ... })
    /// - `"Point^Display::fmt"` → Some(LocalMethodName { `struct_name`: "Point", `trait_name`: Some("Display"), ... })
    /// - `"run"` → None (not a method)
    pub fn parse_qualified(name: &str) -> Option<Self> {
        let local_name = extract_local_name(name);
        Self::parse(local_name)
    }

    /// Returns true if this is a trait method.
    pub fn is_trait_method(&self) -> bool {
        self.trait_name.is_some()
    }
}

/// A unified function identifier that can be either a free function or a method.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FunctionId {
    Free(FreeFunctionName),
    Method(MethodName),
}

impl fmt::Display for FunctionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FunctionId::Free(free) => write!(f, "{free}"),
            FunctionId::Method(method) => write!(f, "{method}"),
        }
    }
}

impl From<FreeFunctionName> for FunctionId {
    fn from(free: FreeFunctionName) -> Self {
        FunctionId::Free(free)
    }
}

impl From<MethodName> for FunctionId {
    fn from(method: MethodName) -> Self {
        FunctionId::Method(method)
    }
}

// =============================================================================
// Type Identifier Types
// =============================================================================

/// A qualified struct type name.
///
/// Format: `{module_path}/{name}`
///
/// Examples:
/// - `./geometry.wado/Point`
/// - `core/internal/SomeType`
///
/// Note: When traits are added to Wado, this may need to evolve into a more
/// general `TypeId` enum (similar to `FunctionId`) to handle trait types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StructName {
    /// The module where the struct is defined
    pub module_source: ModuleSource,
    /// The struct name (e.g., `Point`)
    pub name: String,
}

impl StructName {
    #[must_use]
    pub fn new(module_source: ModuleSource, name: String) -> Self {
        Self {
            module_source,
            name,
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Self {
        Self {
            module_source: ModuleSource::EntryPoint,
            name: name.to_string(),
        }
    }

    /// Create a `StructName` from a module path and name.
    /// This is a convenience method for code that still uses `Vec<String>` paths.
    #[must_use]
    pub fn from_path_and_name(module_path: &[String], name: &str) -> Self {
        Self {
            module_source: ModuleSource::from_path(module_path),
            name: name.to_string(),
        }
    }

    /// Create a `StructName` from string slices.
    /// This is a convenience method for tests and initialization.
    #[must_use]
    pub fn from_strs(module_path: &[&str], name: &str) -> Self {
        let path: Vec<String> = module_path.iter().map(|&s| s.to_string()).collect();
        Self {
            module_source: ModuleSource::from_path(&path),
            name: name.to_string(),
        }
    }
}

impl fmt::Display for StructName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.module_source {
            ModuleSource::EntryPoint => write!(f, "{}", self.name),
            ModuleSource::Core { name: module } => write!(f, "core/{}/{}", module, self.name),
            ModuleSource::Wasi { interface } => write!(f, "wasi/{}/{}", interface, self.name),
            ModuleSource::Local { path } => write!(f, "{}/{}", path, self.name),
        }
    }
}

// =============================================================================
// Parsing Utilities
// =============================================================================

/// Parse a mangled method name back into its components.
///
/// Returns `None` if the format is invalid.
///
/// Expected formats:
/// - `{filename}/{struct_name}::{method_name}`
/// - `{filename}/{struct_name}^{trait_name}::{method_name}`
pub fn parse_method_mangled_name(mangled: &str) -> Option<MethodName> {
    // Split at the last `/` to separate filename from the rest
    let slash_pos = mangled.rfind('/')?;
    let filename = &mangled[..slash_pos];
    let rest = &mangled[slash_pos + 1..];

    // Split at `::` to separate struct/trait from method name
    let double_colon_pos = rest.find("::")?;
    let struct_trait_part = &rest[..double_colon_pos];
    let method_name = &rest[double_colon_pos + 2..];

    // Check for trait separator `^`
    if let Some(caret_pos) = struct_trait_part.find('^') {
        let struct_name = &struct_trait_part[..caret_pos];
        let trait_name = &struct_trait_part[caret_pos + 1..];
        Some(MethodName::new(
            filename.to_string(),
            struct_name.to_string(),
            Some(trait_name.to_string()),
            method_name.to_string(),
        ))
    } else {
        Some(MethodName::new(
            filename.to_string(),
            struct_trait_part.to_string(),
            None,
            method_name.to_string(),
        ))
    }
}

/// Build a core/internal function name.
///
/// Format: `core/internal/{name}`
///
/// Example: `core/internal/log_stdout`
pub fn build_core_internal_name(name: &str) -> FreeFunctionName {
    FreeFunctionName::from_strs(&["core", "internal"], name)
}

// =============================================================================
// Module Path Canonicalization
// =============================================================================

/// Validate that a module path is a valid URI reference.
///
/// Returns `Ok(())` if the path is valid, or `Err(message)` if invalid.
///
/// This should be called by the analyzer before attempting to load a module
/// to provide better error messages.
///
/// # Arguments
/// * `path` - The module path to validate
///
/// # Returns
/// * `Ok(())` - The path is valid
/// * `Err(String)` - The path is invalid, with an error message
pub fn validate_module_path(path: &str) -> Result<(), String> {
    // Special prefixes are always valid
    if path.starts_with("core:")
        || path.starts_with("wasi:")
        || path.starts_with("https://")
        || path.starts_with("http://")
    {
        return Ok(());
    }

    // Try to parse as a URI reference
    match UriRef::parse(path) {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("invalid module path: {e}")),
    }
}

/// Normalize a module path using URI path normalization (RFC 3986).
///
/// This function:
/// - Resolves `.` (current directory) segments
/// - Resolves `..` (parent directory) segments
/// - Removes duplicate slashes
/// - Always uses `/` separator (platform-agnostic)
///
/// The input should be a relative path like `./foo/bar.wado` or `../lib/utils.wado`.
///
/// # Panics
/// Panics if the path is not a valid URI reference.
///
/// Examples:
/// - `./geometry.wado` → `./geometry.wado`
/// - `./sub/../geometry.wado` → `./geometry.wado`
/// - `./sub/./nested/../file.wado` → `./sub/file.wado`
/// - `foo//bar.wado` → `foo/bar.wado`
pub fn normalize_module_path(path: &str) -> String {
    // Handle special module prefixes that shouldn't be normalized
    if path.starts_with("core:")
        || path.starts_with("wasi:")
        || path.starts_with("https://")
        || path.starts_with("http://")
    {
        return path.to_string();
    }

    // RFC 3986 normalize() only removes dot segments from absolute paths,
    // so we use our manual implementation for relative module paths.
    // We still use fluent-uri for validation and encoding normalization.
    let uri_ref =
        UriRef::parse(path).unwrap_or_else(|e| panic!("invalid module path '{path}': {e}"));

    // Apply encoding normalization (percent-encoding, etc.)
    let normalized = uri_ref.normalize();
    // Then apply dot segment removal for relative paths
    remove_dot_segments(normalized.as_str())
}

/// Resolve a relative module path against a base module path.
///
/// This function resolves import paths relative to the importing module's path,
/// producing a canonical path from the project root.
///
/// Examples:
/// - base: `./main.wado`, relative: `./geometry.wado` → `./geometry.wado`
/// - base: `./sub/main.wado`, relative: `./utils.wado` → `./sub/utils.wado`
/// - base: `./sub/main.wado`, relative: `../lib.wado` → `./lib.wado`
/// - base: `./a/b/main.wado`, relative: `../../c.wado` → `./c.wado`
pub fn resolve_module_path(base: &str, relative: &str) -> String {
    // Handle special module prefixes - they don't need resolution
    if relative.starts_with("core:")
        || relative.starts_with("wasi:")
        || relative.starts_with("https://")
        || relative.starts_with("http://")
    {
        return relative.to_string();
    }

    // Get the directory of the base path
    let base_dir = get_parent_path(base);

    // Join the base directory with the relative path
    let joined = if base_dir.is_empty() {
        relative.to_string()
    } else if let Some(stripped) = relative.strip_prefix("./") {
        // ./foo from ./sub/ becomes ./sub/foo
        format!("{base_dir}/{stripped}")
    } else if relative.starts_with("../") {
        // ../foo from ./sub/ needs parent resolution
        format!("{base_dir}/{relative}")
    } else {
        // bare name like "foo.wado" - treat as relative to base dir
        format!("{base_dir}/{relative}")
    };

    // Normalize the result to resolve . and ..
    normalize_module_path(&joined)
}

/// Get the canonical name for an entry point file.
///
/// The entry point file gets a canonical name based on its filename,
/// prefixed with `./` to indicate it's in the project root.
///
/// Example: `main.wado` → `./main.wado`
pub fn canonicalize_entry_point(filename: &str) -> String {
    // Extract just the filename if a path is provided
    let name = filename
        .rsplit('/')
        .next()
        .unwrap_or(filename)
        .rsplit('\\')
        .next()
        .unwrap_or(filename);

    format!("./{name}")
}

/// Convert a filesystem path to a canonical module path.
///
/// This function:
/// - Converts backslashes to forward slashes (Windows compatibility)
/// - Makes the path relative to project root (removes absolute prefix)
/// - Ensures the path starts with `./`
///
/// The `project_root` is the absolute path to the project root directory.
/// The `file_path` is the absolute path to the module file.
///
/// Example:
/// - `project_root`: `/home/user/project`
/// - `file_path`: `/home/user/project/src/lib.wado`
/// - result: `./src/lib.wado`
pub fn filesystem_to_module_path(project_root: &str, file_path: &str) -> Option<String> {
    // Normalize separators to forward slashes
    let root = project_root.replace('\\', "/");
    let path = file_path.replace('\\', "/");

    // Strip the project root prefix
    let relative = path.strip_prefix(&root)?;

    // Remove leading slash if present
    let relative = relative.strip_prefix('/').unwrap_or(relative);

    // Ensure it starts with ./
    if relative.starts_with("./") {
        Some(relative.to_string())
    } else {
        Some(format!("./{relative}"))
    }
}

/// Get the parent directory of a path.
///
/// Given `./sub/file.wado`, returns `./sub`.
/// Given `./file.wado`, returns `.`.
/// Given `file.wado`, returns empty string.
fn get_parent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(pos) => &path[..pos],
        None => "",
    }
}

/// Remove dot segments (`.` and `..`) from a path.
///
/// This implements RFC 3986 Section 5.2.4 for relative paths,
/// which fluent-uri's `normalize()` doesn't handle.
fn remove_dot_segments(path: &str) -> String {
    // Convert backslashes to forward slashes
    let path = path.replace('\\', "/");

    // Split into segments and process
    let mut segments: Vec<&str> = Vec::new();
    let has_leading_dot = path.starts_with("./");

    for segment in path.split('/') {
        match segment {
            "" | "." => {
                // Skip empty segments and current dir markers (except preserving leading ./)
            }
            ".." => {
                // Go up one level if possible
                if !segments.is_empty() && segments.last() != Some(&"..") {
                    segments.pop();
                } else {
                    segments.push("..");
                }
            }
            s => segments.push(s),
        }
    }

    // Reconstruct the path
    let result = segments.join("/");

    // Preserve leading ./ for relative paths
    if has_leading_dot && !result.starts_with("..") {
        format!("./{result}")
    } else if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}

// =============================================================================
// Name Mangling Utilities
// =============================================================================

/// Build a monomorphized type name from base name and type arguments.
///
/// Examples:
/// - `mangle_generic_name("Box", &["i32"])` → `"Box<i32>"`
/// - `mangle_generic_name("Map", &["String", "i32"])` → `"Map<String,i32>"`
pub fn mangle_generic_name(base_name: &str, type_args: &[String]) -> String {
    if type_args.is_empty() {
        base_name.to_string()
    } else {
        format!("{}<{}>", base_name, type_args.join(","))
    }
}

/// Strip type parameters from a generic name, returning the base name.
///
/// This is the inverse of `mangle_generic_name` - it extracts the base name
/// from a potentially generic name.
///
/// Examples:
/// - `strip_type_params("IndexValue<i32>")` → `"IndexValue"`
/// - `strip_type_params("Map<String,i32>")` → `"Map"`
/// - `strip_type_params("Point")` → `"Point"` (unchanged)
#[must_use]
pub fn strip_type_params(name: &str) -> &str {
    if let Some(bracket_pos) = name.find('<') {
        &name[..bracket_pos]
    } else {
        name
    }
}

/// Build a monomorphized method name from struct name, type args, and method name.
///
/// Examples:
/// - `mangle_method_generic("Box", &["i32"], "get")` → `"Box<i32>::get"`
/// - `mangle_method_generic("Array", &["String"], "len")` → `"Array<String>::len"`
pub fn mangle_method_generic(struct_name: &str, type_args: &[String], method_name: &str) -> String {
    let mangled_struct = mangle_generic_name(struct_name, type_args);
    format!("{mangled_struct}::{method_name}")
}

/// Build a function type name from parameter count and return type name.
///
/// Examples:
/// - `mangle_fn_type(2, "i32")` → `"Fn<2,i32>"`
/// - `mangle_fn_type(0, "String")` → `"Fn<0,String>"`
pub fn mangle_fn_type(param_count: usize, ret_type: &str) -> String {
    format!("Fn<{param_count},{ret_type}>")
}

/// Build a tuple type name from element type names.
///
/// Examples:
/// - `mangle_tuple_type(&["i32", "String"])` → `"Tuple<i32,String>"`
/// - `mangle_tuple_type(&["i32"])` → `"Tuple<i32>"`
pub fn mangle_tuple_type(elem_types: &[String]) -> String {
    format!("Tuple<{}>", elem_types.join(","))
}

/// Build an Option type name from inner type name.
///
/// Examples:
/// - `mangle_option_type("i32")` → `"Option<i32>"`
pub fn mangle_option_type(inner_type: &str) -> String {
    format!("Option<{inner_type}>")
}

/// Build an Array type name from element type name.
///
/// Examples:
/// - `mangle_array_type("i32")` → `"Array<i32>"`
pub fn mangle_array_type(elem_type: &str) -> String {
    format!("Array<{elem_type}>")
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Method Name Tests
    // =========================================================================

    #[test]
    fn test_method_name_to_string_simple() {
        let method = MethodName::new(
            "./geometry.wado".to_string(),
            "Point".to_string(),
            None,
            "sum".to_string(),
        );
        assert_eq!(method.to_string(), "./geometry.wado/Point::sum");
    }

    #[test]
    fn test_method_name_to_string_with_trait() {
        let method = MethodName::new(
            "./geometry.wado".to_string(),
            "Point".to_string(),
            Some("Display".to_string()),
            "fmt".to_string(),
        );
        assert_eq!(method.to_string(), "./geometry.wado/Point^Display::fmt");
    }

    #[test]
    fn test_parse_method_mangled_name_simple() {
        let parsed = parse_method_mangled_name("./geometry.wado/Point::sum").unwrap();
        assert_eq!(parsed.filename, "./geometry.wado");
        assert_eq!(parsed.struct_name, "Point");
        assert_eq!(parsed.trait_name, None);
        assert_eq!(parsed.method_name, "sum");
    }

    #[test]
    fn test_parse_method_mangled_name_with_trait() {
        let parsed = parse_method_mangled_name("./geometry.wado/Point^Display::fmt").unwrap();
        assert_eq!(parsed.filename, "./geometry.wado");
        assert_eq!(parsed.struct_name, "Point");
        assert_eq!(parsed.trait_name, Some("Display".to_string()));
        assert_eq!(parsed.method_name, "fmt");
    }

    #[test]
    fn test_roundtrip_simple() {
        let original = MethodName::new(
            "./geometry.wado".to_string(),
            "Point".to_string(),
            None,
            "magnitude".to_string(),
        );
        let mangled = original.to_string();
        let parsed = parse_method_mangled_name(&mangled).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_roundtrip_with_trait() {
        let original = MethodName::new(
            "./models/user.wado".to_string(),
            "User".to_string(),
            Some("Serialize".to_string()),
            "to_json".to_string(),
        );
        let mangled = original.to_string();
        let parsed = parse_method_mangled_name(&mangled).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_parse_invalid_no_slash() {
        assert!(parse_method_mangled_name("Point::sum").is_none());
    }

    #[test]
    fn test_parse_invalid_no_double_colon() {
        assert!(parse_method_mangled_name("./geometry.wado/Point").is_none());
    }

    // =========================================================================
    // Free Function Name Tests
    // =========================================================================

    #[test]
    fn test_free_function_name_to_string() {
        let func = FreeFunctionName::from_path_and_name(
            &["core".to_string(), "cli".to_string()],
            "println",
        );
        assert_eq!(func.to_string(), "core/cli/println");
    }

    #[test]
    fn test_free_function_name_from_strs() {
        let func = FreeFunctionName::from_strs(&["core", "internal"], "log_stdout");
        assert_eq!(func.to_string(), "core/internal/log_stdout");
    }

    #[test]
    fn test_free_function_name_empty_path() {
        let func = FreeFunctionName::from_strs(&[], "main");
        assert_eq!(func.to_string(), "main");
    }

    // =========================================================================
    // Struct Name Tests
    // =========================================================================

    #[test]
    fn test_struct_name_to_string() {
        let struct_name = StructName::from_path_and_name(&["./geometry.wado".to_string()], "Point");
        assert_eq!(struct_name.to_string(), "./geometry.wado/Point");
    }

    #[test]
    fn test_struct_name_from_strs() {
        let struct_name = StructName::from_strs(&["core", "internal"], "SomeType");
        assert_eq!(struct_name.to_string(), "core/internal/SomeType");
    }

    #[test]
    fn test_struct_name_empty_path() {
        let struct_name = StructName::from_path_and_name(&[], "Point");
        assert_eq!(struct_name.to_string(), "Point");
    }

    #[test]
    fn test_struct_name_hash_eq() {
        use std::collections::HashSet;
        let s1 = StructName::from_path_and_name(&["./geometry.wado".to_string()], "Point");
        let s2 = StructName::from_path_and_name(&["./geometry.wado".to_string()], "Point");
        let s3 = StructName::from_path_and_name(&["./other.wado".to_string()], "Point");

        let mut set = HashSet::new();
        set.insert(s1.clone());
        assert!(set.contains(&s2));
        assert!(!set.contains(&s3));
    }

    // =========================================================================
    // Core Internal Name Tests
    // =========================================================================

    #[test]
    fn test_build_core_internal_name() {
        let name = build_core_internal_name("log_stdout");
        assert_eq!(name.to_string(), "core/internal/log_stdout");
        assert_eq!(name.module_path, vec!["core", "internal"]);
        assert_eq!(name.name, "log_stdout");
    }

    // =========================================================================
    // Module Path Canonicalization Tests
    // =========================================================================

    #[test]
    fn test_normalize_simple_path() {
        assert_eq!(normalize_module_path("./geometry.wado"), "./geometry.wado");
        assert_eq!(normalize_module_path("./sub/file.wado"), "./sub/file.wado");
    }

    #[test]
    fn test_normalize_dot_segments() {
        assert_eq!(
            normalize_module_path("./sub/../geometry.wado"),
            "./geometry.wado"
        );
        assert_eq!(
            normalize_module_path("./sub/./file.wado"),
            "./sub/file.wado"
        );
        assert_eq!(normalize_module_path("./a/b/../c/./d.wado"), "./a/c/d.wado");
    }

    #[test]
    fn test_normalize_special_prefixes() {
        // Special prefixes should not be modified
        assert_eq!(normalize_module_path("core:cli"), "core:cli");
        assert_eq!(normalize_module_path("wasi:filesystem"), "wasi:filesystem");
        assert_eq!(
            normalize_module_path("https://example.com/lib.wado"),
            "https://example.com/lib.wado"
        );
    }

    #[test]
    fn test_resolve_same_directory() {
        assert_eq!(
            resolve_module_path("./main.wado", "./geometry.wado"),
            "./geometry.wado"
        );
    }

    #[test]
    fn test_resolve_subdirectory() {
        assert_eq!(
            resolve_module_path("./sub/main.wado", "./utils.wado"),
            "./sub/utils.wado"
        );
        assert_eq!(
            resolve_module_path("./a/b/main.wado", "./file.wado"),
            "./a/b/file.wado"
        );
    }

    #[test]
    fn test_resolve_parent_directory() {
        assert_eq!(
            resolve_module_path("./sub/main.wado", "../lib.wado"),
            "./lib.wado"
        );
        assert_eq!(
            resolve_module_path("./a/b/main.wado", "../../c.wado"),
            "./c.wado"
        );
    }

    #[test]
    fn test_resolve_special_prefixes() {
        // Special prefixes should pass through unchanged
        assert_eq!(
            resolve_module_path("./sub/main.wado", "core:cli"),
            "core:cli"
        );
        assert_eq!(
            resolve_module_path("./sub/main.wado", "wasi:filesystem"),
            "wasi:filesystem"
        );
    }

    #[test]
    fn test_canonicalize_entry_point() {
        assert_eq!(canonicalize_entry_point("main.wado"), "./main.wado");
        assert_eq!(
            canonicalize_entry_point("/absolute/path/main.wado"),
            "./main.wado"
        );
        assert_eq!(
            canonicalize_entry_point("C:\\Windows\\path\\main.wado"),
            "./main.wado"
        );
    }

    #[test]
    fn test_filesystem_to_module_path() {
        assert_eq!(
            filesystem_to_module_path("/home/user/project", "/home/user/project/src/lib.wado"),
            Some("./src/lib.wado".to_string())
        );
        assert_eq!(
            filesystem_to_module_path("/home/user/project", "/home/user/project/main.wado"),
            Some("./main.wado".to_string())
        );
    }

    #[test]
    fn test_filesystem_to_module_path_windows() {
        assert_eq!(
            filesystem_to_module_path(
                "C:\\Users\\dev\\project",
                "C:\\Users\\dev\\project\\src\\lib.wado"
            ),
            Some("./src/lib.wado".to_string())
        );
    }

    #[test]
    fn test_get_parent_path() {
        assert_eq!(get_parent_path("./sub/file.wado"), "./sub");
        assert_eq!(get_parent_path("./file.wado"), ".");
        assert_eq!(get_parent_path("file.wado"), "");
    }

    #[test]
    fn test_remove_dot_segments() {
        assert_eq!(remove_dot_segments("./a/b/../c.wado"), "./a/c.wado");
        assert_eq!(remove_dot_segments("./a/./b/c.wado"), "./a/b/c.wado");
        assert_eq!(remove_dot_segments("a//b/c.wado"), "a/b/c.wado");
    }

    // =========================================================================
    // Module Path Validation Tests
    // =========================================================================

    #[test]
    fn test_validate_module_path_valid() {
        assert!(validate_module_path("./geometry.wado").is_ok());
        assert!(validate_module_path("../lib.wado").is_ok());
        assert!(validate_module_path("core:cli").is_ok());
        assert!(validate_module_path("wasi:filesystem").is_ok());
        assert!(validate_module_path("https://example.com/lib.wado").is_ok());
        assert!(validate_module_path("http://localhost:8080/lib.wado").is_ok());
    }

    #[test]
    fn test_validate_module_path_invalid() {
        // Paths with invalid URI characters should fail
        // Note: Most printable characters are valid in URI references,
        // so we test with control characters or invalid sequences
        assert!(validate_module_path("./file with\x00null.wado").is_err());
    }

    // =========================================================================
    // ModuleSource Tests
    // =========================================================================

    #[test]
    fn test_module_source_from_path_core() {
        let source = ModuleSource::from_path(&["core".to_string(), "prelude".to_string()]);
        assert!(matches!(source, ModuleSource::Core { name } if name == "prelude"));

        let source = ModuleSource::from_path(&["core".to_string(), "cli".to_string()]);
        assert!(matches!(source, ModuleSource::Core { name } if name == "cli"));

        let source = ModuleSource::from_path(&["core".to_string(), "internal".to_string()]);
        assert!(source.is_core_internal());
    }

    #[test]
    fn test_module_source_from_path_wasi() {
        let source = ModuleSource::from_path(&["wasi".to_string(), "cli".to_string()]);
        assert!(matches!(source, ModuleSource::Wasi { interface } if interface == "cli"));

        let source = ModuleSource::from_path(&["wasi".to_string(), "io".to_string()]);
        assert!(source.is_wasi());
    }

    #[test]
    fn test_module_source_from_path_local() {
        let source = ModuleSource::from_path(&["./geometry.wado".to_string()]);
        assert!(matches!(source, ModuleSource::Local { path } if path == "./geometry.wado"));

        let source = ModuleSource::from_path(&["../lib.wado".to_string()]);
        assert!(source.is_local());
    }

    #[test]
    fn test_module_source_from_path_entry_point() {
        let source = ModuleSource::from_path(&[]);
        assert!(matches!(source, ModuleSource::EntryPoint));
    }

    #[test]
    fn test_module_source_to_path() {
        let source = ModuleSource::core("prelude");
        assert_eq!(source.to_path(), vec!["core", "prelude"]);

        let source = ModuleSource::wasi("cli");
        assert_eq!(source.to_path(), vec!["wasi", "cli"]);

        let source = ModuleSource::local("./geometry.wado");
        assert_eq!(source.to_path(), vec!["./geometry.wado"]);

        let source = ModuleSource::EntryPoint;
        assert!(source.to_path().is_empty());
    }

    #[test]
    fn test_module_source_display() {
        assert_eq!(ModuleSource::core("prelude").to_string(), "core:prelude");
        assert_eq!(ModuleSource::wasi("cli").to_string(), "wasi:cli");
        assert_eq!(
            ModuleSource::local("./geometry.wado").to_string(),
            "./geometry.wado"
        );
        assert_eq!(ModuleSource::EntryPoint.to_string(), "<entry>");
    }

    #[test]
    fn test_module_source_helpers() {
        let core = ModuleSource::core("internal");
        assert!(core.is_core());
        assert!(core.is_core_internal());
        assert!(!core.is_wasi());
        assert!(!core.is_local());

        let builtin = ModuleSource::core("builtin");
        assert!(builtin.is_core_builtin());

        let prelude = ModuleSource::core("prelude");
        assert!(prelude.is_core_prelude());

        let wasi = ModuleSource::wasi("cli");
        assert!(wasi.is_wasi());
        assert!(!wasi.is_core());

        let local = ModuleSource::local("./file.wado");
        assert!(local.is_local());
        assert!(!local.is_core());
    }

    #[test]
    fn test_module_source_roundtrip() {
        // Test that from_path and to_path are inverses (for supported formats)
        let paths = vec![
            vec!["core".to_string(), "prelude".to_string()],
            vec!["wasi".to_string(), "cli".to_string()],
            vec!["./geometry.wado".to_string()],
        ];

        for path in paths {
            let source = ModuleSource::from_path(&path);
            assert_eq!(source.to_path(), path, "Roundtrip failed for {path:?}");
        }
    }
}
