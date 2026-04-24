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
///
/// Note: Two `EntryPoint` variants are considered equal regardless of their
/// `filename` field. This ensures that types defined in the entry module
/// are consistent across different compilation phases.
#[derive(Debug, Clone)]
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
    /// Remote module loaded via HTTP/HTTPS
    Remote {
        /// Full URL (e.g., "<https://example.com/lib.wado>")
        url: String,
    },
    /// Entry point module (the main file being compiled)
    EntryPoint {
        /// Filename of the entry point (e.g., "hello.wado", "<stdin>", "<entry>")
        filename: String,
    },
}

impl PartialEq for ModuleSource {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Core { name: a }, Self::Core { name: b }) => a == b,
            (Self::Wasi { interface: a }, Self::Wasi { interface: b }) => a == b,
            (Self::Local { path: a }, Self::Local { path: b }) => a == b,
            (Self::Remote { url: a }, Self::Remote { url: b }) => a == b,
            // Entry points are equal regardless of filename
            (Self::EntryPoint { .. }, Self::EntryPoint { .. }) => true,
            _ => false,
        }
    }
}

impl Eq for ModuleSource {}

impl std::hash::Hash for ModuleSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Use discriminant to differentiate variants
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Core { name } => name.hash(state),
            Self::Wasi { interface } => interface.hash(state),
            Self::Local { path } => path.hash(state),
            Self::Remote { url } => url.hash(state),
            // Entry points hash the same regardless of filename
            Self::EntryPoint { .. } => {}
        }
    }
}

impl Default for ModuleSource {
    /// Placeholder value — replaced by the link phase with the real module source.
    fn default() -> Self {
        Self::Core {
            name: String::new(),
        }
    }
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

    /// Create a remote module source.
    #[must_use]
    pub fn remote(url: impl Into<String>) -> Self {
        Self::Remote { url: url.into() }
    }

    /// `core:prelude` — the prelude module.
    #[must_use]
    pub fn prelude() -> Self {
        Self::core("prelude")
    }

    /// `core:prelude/string.wado` — the String type.
    #[must_use]
    pub fn string() -> Self {
        Self::core("prelude/string.wado")
    }

    /// `core:prelude/array.wado` — the Array type.
    #[must_use]
    pub fn array() -> Self {
        Self::core("prelude/array.wado")
    }

    /// `core:prelude/format.wado` — format trait helpers.
    #[must_use]
    pub fn format() -> Self {
        Self::core("prelude/format.wado")
    }

    /// `core:prelude/int128.wado` — 128-bit integer types.
    #[must_use]
    pub fn int128() -> Self {
        Self::core("prelude/int128.wado")
    }

    /// `core:prelude/primitive.wado` — primitive type methods.
    #[must_use]
    pub fn primitive() -> Self {
        Self::core("prelude/primitive.wado")
    }

    /// `core:prelude/types.wado` — core type definitions.
    #[must_use]
    pub fn types() -> Self {
        Self::core("prelude/types.wado")
    }

    /// `core:prelude/traits.wado` — builtin trait definitions.
    #[must_use]
    pub fn traits() -> Self {
        Self::core("prelude/traits.wado")
    }

    /// `core:internal` — compiler internal functions.
    #[must_use]
    pub fn internal() -> Self {
        Self::core("internal")
    }

    /// `core:allocator` — linear memory allocator (compiled into "mem" Wasm module).
    #[must_use]
    pub fn allocator() -> Self {
        Self::core("allocator")
    }

    /// `core:builtin` — builtin wasm instruction mappings.
    #[must_use]
    pub fn builtin() -> Self {
        Self::core("builtin")
    }

    /// `core:cli` — CLI output functions.
    #[must_use]
    pub fn cli() -> Self {
        Self::core("cli")
    }

    /// Create an entry point module source with a filename.
    #[must_use]
    pub fn entry_point_with_filename(filename: impl Into<String>) -> Self {
        Self::EntryPoint {
            filename: filename.into(),
        }
    }

    /// Convert from a legacy `Vec<String>` module path.
    ///
    /// This enables gradual migration from the old representation.
    #[must_use]
    pub fn from_path(path: &[String]) -> Self {
        match path {
            // Legacy: empty path represents entry module
            // TODO: Remove this case by changing resolve_all_modules to return IndexMap<ModuleSource, _>
            [] => Self::entry_point_with_filename("<entry>"),
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
            Self::Remote { url } => vec![url.clone()],
            Self::EntryPoint { filename } => vec![filename.clone()],
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

    /// Check if this is a remote module.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
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

    /// Check if this is the entry point module.
    #[must_use]
    pub fn is_entry_point(&self) -> bool {
        matches!(self, Self::EntryPoint { .. })
    }

    /// Check if this looks like an effect module (single `PascalCase` name).
    /// Effects are represented as Local paths with a single element like "Stdout".
    #[must_use]
    pub fn is_effect_like(&self) -> bool {
        if self.is_entry_point() {
            return false;
        }
        let path = self.to_path();
        path.len() == 1
            && path[0]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
            && !path[0].contains('/')
            && !path[0].contains('.')
    }

    /// Get the effect name if this is an effect-like module.
    #[must_use]
    pub fn effect_name(&self) -> Option<String> {
        if self.is_effect_like() {
            let path = self.to_path();
            path.into_iter().next()
        } else {
            None
        }
    }

    /// Convert to a path string format used for method name mangling.
    ///
    /// Returns `self.to_path().join("/")`:
    /// - `EntryPoint { filename }` → `"{filename}"`
    /// - `Local { path }` → `"{path}"`
    /// - `Core { name }` → `"core/{name}"`
    /// - `Wasi { interface }` → `"wasi/{interface}"`
    /// - `Remote { url }` → `"{url}"`
    #[must_use]
    pub fn to_path_string(&self) -> String {
        self.to_path().join("/")
    }

    /// Create a module-qualified name using `//` as separator.
    ///
    /// The `//` separator cannot appear in file paths, making it safe
    /// for disambiguating same-named types from different modules.
    ///
    /// Examples:
    /// - `ModuleSource::prelude().qualify_name("Option")` → `"core:prelude//Option"`
    /// - `ModuleSource::local("./geometry.wado").qualify_name("Point")` → `"./geometry.wado//Point"`
    /// - `ModuleSource::entry_point_with_filename("main.wado").qualify_name("Foo")` → `"main.wado//Foo"`
    #[must_use]
    pub fn qualify_name(&self, name: &str) -> String {
        format!("{self}//{name}")
    }

    /// Return a filename suitable for diagnostic messages.
    ///
    /// Returns an empty string for entry points without real filenames
    /// (e.g., `<stdin>`, `<entry>`) so that `Logger::apply_file_context`
    /// can fill in the correct file from the logger's current file context.
    #[must_use]
    pub fn diagnostic_filename(&self) -> String {
        match self {
            Self::EntryPoint { filename } => {
                if filename.starts_with('<') {
                    String::new() // synthetic names like <stdin>, <entry>
                } else {
                    filename.clone()
                }
            }
            other => other.to_string(),
        }
    }
}

impl fmt::Display for ModuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core { name } => write!(f, "core:{name}"),
            Self::Wasi { interface } => write!(f, "wasi:{interface}"),
            Self::Local { path } => write!(f, "{path}"),
            Self::Remote { url } => write!(f, "{url}"),
            Self::EntryPoint { filename } => {
                write!(f, "{filename}")
            }
        }
    }
}

/// A free function name (not a method on a struct).
///
/// Format: `{module_source}/{name}`
///
/// Examples:
/// - `./geometry.wado/helper`
/// - `core/internal/log_stdout`
#[derive(Debug, Clone)]
pub struct FreeFunctionName {
    /// The module where the function is defined
    pub module_source: ModuleSource,
    /// The function name (e.g., `helper`)
    pub name: String,
    /// Whether this function is monomorphized (instantiated from a generic)
    pub is_monomorphized: bool,
    /// Base generic name if monomorphized (e.g., "Array" for "Array<i32>`::len`")
    pub base_name: Option<String>,
}

// Manually implement Hash/Eq to only use module_source and name (not metadata)
impl Hash for FreeFunctionName {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.module_source.hash(state);
        self.name.hash(state);
    }
}

impl PartialEq for FreeFunctionName {
    fn eq(&self, other: &Self) -> bool {
        self.module_source == other.module_source && self.name == other.name
    }
}

impl Eq for FreeFunctionName {}

impl FreeFunctionName {
    pub fn new(module_source: ModuleSource, name: String) -> Self {
        Self {
            module_source,
            name,
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` from a module path and name.
    /// This is a convenience method for code that still uses `Vec<String>` paths.
    pub fn from_path_and_name(module_path: &[String], name: &str) -> Self {
        Self {
            module_source: ModuleSource::from_path(module_path),
            name: name.to_string(),
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` from string literal slices.
    /// Convenience method for when you have &[&str] instead of &[String].
    pub fn from_strs(module_path: &[&str], name: &str) -> Self {
        let path: Vec<String> = module_path.iter().map(|s| (*s).to_string()).collect();
        Self {
            module_source: ModuleSource::from_path(&path),
            name: name.to_string(),
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` from `ModuleSource` and name.
    pub fn from_module_source(module_source: &ModuleSource, name: &str) -> Self {
        Self {
            module_source: module_source.clone(),
            name: name.to_string(),
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` with monomorphization metadata.
    pub fn with_monomorph_info(
        module_source: ModuleSource,
        name: String,
        base_name: String,
    ) -> Self {
        Self {
            module_source,
            name,
            is_monomorphized: true,
            base_name: Some(base_name),
        }
    }
}

impl fmt::Display for FreeFunctionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.module_source {
            ModuleSource::EntryPoint { .. } => write!(f, "{}", self.name),
            ModuleSource::Core { name: module } => write!(f, "core/{}/{}", module, self.name),
            ModuleSource::Wasi { interface } => write!(f, "wasi/{}/{}", interface, self.name),
            ModuleSource::Local { path } => write!(f, "{}/{}", path, self.name),
            ModuleSource::Remote { url } => write!(f, "{}/{}", url, self.name),
        }
    }
}

/// A method name on a struct.
///
/// Format:
/// - Without trait: `{filename}/{struct_name}::{method_name}`
/// - With trait: `{module_source}/{struct_name}^{trait_name}::{method_name}`
///
/// Examples:
/// - `./geometry.wado/Point::sum`
/// - `./geometry.wado/Point^Display::fmt`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodName {
    /// The module source where the method is defined
    pub module_source: ModuleSource,
    /// The struct name (e.g., `Point`)
    pub struct_name: String,
    /// The trait name if this is a trait implementation (e.g., `Display`)
    pub trait_name: Option<String>,
    /// The method name (e.g., `sum`)
    pub method_name: String,
}

impl MethodName {
    pub fn new(
        module_source: ModuleSource,
        struct_name: String,
        trait_name: Option<String>,
        method_name: String,
    ) -> Self {
        Self {
            module_source,
            struct_name,
            trait_name,
            method_name,
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
            mangle_ref_aware(struct_name, type_args)
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
        // For entry point, don't include the module prefix
        let prefix = if self.module_source.is_entry_point() {
            String::new()
        } else {
            format!("{}/", self.module_source.to_path_string())
        };

        match &self.trait_name {
            Some(trait_name) => {
                write!(
                    f,
                    "{}{}^{}::{}",
                    prefix, self.struct_name, trait_name, self.method_name
                )
            }
            None => {
                write!(f, "{}{}::{}", prefix, self.struct_name, self.method_name)
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
/// - `"core/string/String::len"` → `"String::len"`
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
/// - `Point::sum` → `struct_name="Point"`, `trait_name=None`, `method_name="sum"`
/// - `Point^Display::fmt` → `struct_name="Point"`, `trait_name=Some("Display")`, `method_name="fmt"`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalMethodName {
    /// The struct name, possibly with type args (e.g., "Point" or "Point<i32>")
    pub struct_name: String,
    /// The base struct name without type args (e.g., "Point")
    /// This is preserved during monomorphization for lookup purposes.
    pub base_struct_name: String,
    /// The trait name if this is a trait method (e.g., "Display")
    pub trait_name: Option<String>,
    /// The method name (e.g., "sum" or "fmt")
    pub method_name: String,
    /// Method-level type args (e.g., ["i64"] for transform<i64>)
    pub method_type_args: Vec<String>,
    /// Whether the struct name is a type parameter that should be substituted directly
    /// during monomorphization (e.g., `T^Ord::cmp` where T should become i32).
    pub is_type_param_receiver: bool,
    /// Whether this method is from an `impl Trait for &T` or `impl Trait for &mut T`.
    /// When true, the function name uses the inner type name (e.g., "Array") but the
    /// actual impl is on the reference type (e.g., &Array<T>).
    pub is_ref_impl: bool,
    /// CM canonical name from `#[cm("...")]` attribute on resource methods.
    /// When set, synthesis generates a CM binding function and rewrites
    /// the call site to use it instead of the original resource method.
    pub cm_name: Option<String>,
}

impl LocalMethodName {
    /// Create a new `LocalMethodName` directly from components.
    ///
    /// IMPORTANT: `struct_name` must be the base struct name WITHOUT type parameters.
    /// Use `with_type_args()` or `with_struct_type_args()` to add type parameters.
    #[must_use]
    pub fn new(struct_name: String, trait_name: Option<String>, method_name: String) -> Self {
        debug_assert!(
            !struct_name.contains('<'),
            "LocalMethodName::new() expects base struct name without type params, got: {struct_name}"
        );
        Self {
            base_struct_name: struct_name.clone(),
            struct_name,
            trait_name,
            method_name,
            method_type_args: vec![],
            is_type_param_receiver: false,
            is_ref_impl: false,
            cm_name: None,
        }
    }

    /// Create a new `LocalMethodName` with all components including method type args.
    ///
    /// IMPORTANT: `struct_name` must be the base struct name WITHOUT type parameters.
    /// Use `with_type_args()` to add struct type parameters.
    #[must_use]
    pub fn with_method_type_args(
        struct_name: String,
        trait_name: Option<String>,
        method_name: String,
        method_type_args: Vec<String>,
    ) -> Self {
        debug_assert!(
            !struct_name.contains('<'),
            "LocalMethodName::with_method_type_args() expects base struct name without type params, got: {struct_name}"
        );
        Self {
            base_struct_name: struct_name.clone(),
            struct_name,
            trait_name,
            method_name,
            method_type_args,
            is_type_param_receiver: false,
            is_ref_impl: false,
            cm_name: None,
        }
    }

    /// Create a version of this `LocalMethodName` with type args applied.
    ///
    /// `impl_type_args` are applied to the struct name (e.g., "Array" + ["i32"] → "Array<i32>").
    /// `method_type_args` are stored separately (not embedded in `method_name`).
    /// `base_struct_name` is preserved (not changed by type args).
    #[must_use]
    pub fn with_type_args(&self, impl_type_args: &[String], method_type_args: &[String]) -> Self {
        let mangled_struct = if impl_type_args.is_empty() {
            self.struct_name.clone()
        } else {
            mangle_ref_aware(&self.base_struct_name, impl_type_args)
        };
        Self {
            struct_name: mangled_struct,
            base_struct_name: self.base_struct_name.clone(),
            trait_name: self.trait_name.clone(),
            method_name: self.method_name.clone(),
            method_type_args: method_type_args.to_vec(),
            is_type_param_receiver: self.is_type_param_receiver,
            is_ref_impl: self.is_ref_impl,
            cm_name: self.cm_name.clone(),
        }
    }

    /// Create a version with only struct type args (no method type args).
    /// This is a convenience method for the common case.
    #[must_use]
    pub fn with_struct_type_args(&self, type_args: &[String]) -> Self {
        self.with_type_args(type_args, &[])
    }

    /// Create a version with the struct name directly substituted (not wrapped with type args).
    /// Used when the struct name is a type parameter (e.g., `T^Ord::cmp` → `i32^Ord::cmp`).
    ///
    /// `new_name` is the full mangled name (e.g., `"Option<String>"`).
    /// `base_name` is the name without type parameters (e.g., `"Option"`).
    #[must_use]
    pub fn with_substituted_struct_name(&self, new_name: &str, base_name: &str) -> Self {
        Self {
            struct_name: new_name.to_string(),
            base_struct_name: base_name.to_string(),
            trait_name: self.trait_name.clone(),
            method_name: self.method_name.clone(),
            method_type_args: self.method_type_args.clone(),
            is_type_param_receiver: false,
            is_ref_impl: self.is_ref_impl,
            cm_name: self.cm_name.clone(),
        }
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

    /// Generate the mangled name from the components.
    ///
    /// Produces:
    /// - `StructName::method` for inherent methods
    /// - `StructName^TraitName::method` for trait methods
    /// - `StructName<TypeArgs>::method` for monomorphized methods
    /// - `StructName<TypeArgs>^TraitName::method` for monomorphized trait methods
    #[must_use]
    pub fn to_mangled_name(&self) -> String {
        let method_part = self.full_method_name();
        if let Some(trait_name) = &self.trait_name {
            format!("{}^{}::{}", self.struct_name, trait_name, method_part)
        } else {
            format!("{}::{}", self.struct_name, method_part)
        }
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
        write!(f, "{}/{}", self.module_source, self.name)
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

/// Resolve an import source to a `ModuleSource`.
///
/// This is the primary function for resolving import paths to module identifiers.
///
/// # Arguments
/// * `from_module` - The `ModuleSource` of the importing module
/// * `import_source` - The import source string (e.g., `"./geometry.wado"` or `"core:cli"`)
///
/// # Returns
/// The resolved `ModuleSource`.
pub fn resolve_import(from_module: &ModuleSource, import_source: &str) -> ModuleSource {
    resolve_import_with_entry(from_module, import_source, None)
}

/// Resolve an import source, consulting a Kiln [`crate::kiln::InvocationIndex`]
/// first.
///
/// When the `(from_module, import_source)` pair matches a recorded invocation,
/// the returned [`ModuleSource`] points at the invocation's generated entry
/// module (under `build/kiln/…`). Otherwise falls back to
/// [`resolve_import_with_entry`] unchanged.
///
/// Call this in place of [`resolve_import`] wherever an `InvocationIndex` is
/// available — typically the CLI and LSP compile entry points, after the
/// Kiln pipeline has populated the index.
pub fn resolve_import_with_invocations(
    from_module: &ModuleSource,
    import_source: &str,
    entry_module: Option<&ModuleSource>,
    invocations: &crate::kiln::InvocationIndex,
) -> ModuleSource {
    if !invocations.is_empty() {
        let decl_file = match from_module {
            ModuleSource::Local { path } => path.as_str(),
            ModuleSource::EntryPoint { filename } => filename.as_str(),
            _ => "",
        };
        if let Some(entry_path) = invocations.redirect(decl_file, import_source) {
            return ModuleSource::Local {
                path: normalize_module_path(entry_path),
            };
        }
    }
    resolve_import_with_entry(from_module, import_source, entry_module)
}

pub fn resolve_import_with_entry(
    from_module: &ModuleSource,
    import_source: &str,
    entry_module: Option<&ModuleSource>,
) -> ModuleSource {
    // Handle special prefixes
    if let Some(name) = import_source.strip_prefix("core:") {
        return ModuleSource::Core {
            name: name.to_string(),
        };
    }
    if let Some(interface) = import_source.strip_prefix("wasi:") {
        return ModuleSource::Wasi {
            interface: interface.to_string(),
        };
    }
    if import_source.starts_with("https://") || import_source.starts_with("http://") {
        return ModuleSource::Remote {
            url: import_source.to_string(),
        };
    }

    // Handle relative imports from local modules
    // For entry points, we don't resolve against the filename - just use the import directly
    if let ModuleSource::Local { path: from_path } = from_module
        && (from_path.starts_with("./") || from_path.starts_with("../"))
    {
        let resolved = resolve_module_path(from_path, import_source);
        // If this resolves to the entry module's canonical name, return the
        // entry ModuleSource to maintain a single type identity.
        if let Some(entry) = entry_module {
            let entry_canonical = match entry {
                ModuleSource::EntryPoint { filename } => canonicalize_entry_point(filename),
                _ => entry.to_string(),
            };
            if resolved == entry_canonical {
                return entry.clone();
            }
        }
        return ModuleSource::Local { path: resolved };
    }

    // Fallback: normalize and return as Local path
    // This handles EntryPoint imports and bare imports
    ModuleSource::Local {
        path: normalize_module_path(import_source),
    }
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
    let has_leading_slash = path.starts_with('/');

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
    } else if has_leading_slash {
        format!("/{result}")
    } else if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}

/// Information about a type for name formatting.
///
/// This enum represents the structure of a type without requiring
/// knowledge of `TypeId` or `ResolvedType`. It serves as the interface
/// between type resolution (in tir.rs) and name formatting (in name.rs).
#[derive(Debug, Clone)]
pub enum TypeNameInfo {
    /// A primitive type (i32, f64, bool, etc.)
    Primitive(String),
    /// The unit type ()
    Unit,
    /// A named type (struct, enum, variant, resource, newtype, type param)
    Named(String),
    /// A generic instance with type argument names already resolved
    Generic { name: String, args: Vec<String> },
    /// Option<T> with inner type name
    Option(String),
    /// A function type with param count and return type name
    Function {
        param_count: usize,
        return_type: String,
    },
    /// `builtin::array<T>` (raw Wasm GC array, NOT the user-facing `Array<T>` struct)
    BuiltinArray(String),
    /// Reactive<T> with inner type name
    Reactive(String),
    /// A reference type - formats as inner type (references stripped)
    Ref(String),
    /// Never, Unknown, or Error types
    Unknown,
}

/// Format a type name from its structural info.
///
/// This function centralizes all type name formatting logic.
/// Other modules should use this instead of formatting type names directly.
#[must_use]
pub fn format_type_name(info: TypeNameInfo) -> String {
    match info {
        TypeNameInfo::Primitive(name) => name,
        TypeNameInfo::Unit => "unit".to_string(),
        TypeNameInfo::Named(name) => name,
        TypeNameInfo::Generic { name, args } => mangle_generic_name(&name, &args),
        TypeNameInfo::Option(inner) => mangle_option_type(&inner),
        TypeNameInfo::Function {
            param_count,
            return_type,
        } => mangle_fn_type(param_count, &return_type),
        TypeNameInfo::BuiltinArray(elem) => mangle_builtin_array_type(&elem),
        TypeNameInfo::Reactive(inner) => mangle_generic_name("Reactive", &[inner]),
        TypeNameInfo::Ref(inner) => inner,
        TypeNameInfo::Unknown => "unknown".to_string(),
    }
}

/// Build a mangled name that handles reference prefixes correctly.
///
/// For `&` and `&mut` base names, formats as prefix: `&Array<i32>`, `&mut Array<i32>`.
/// For other base names, formats as generic: `Array<i32>`, `Map<String,i32>`.
fn mangle_ref_aware(base_name: &str, type_args: &[String]) -> String {
    if (base_name == "&" || base_name == "&mut") && type_args.len() == 1 {
        if base_name == "&mut" {
            format!("&mut {}", type_args[0])
        } else {
            format!("&{}", type_args[0])
        }
    } else {
        mangle_generic_name(base_name, type_args)
    }
}

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

/// Build an Option type name from inner type name.
///
/// Examples:
/// - `mangle_option_type("i32")` → `"Option<i32>"`
pub fn mangle_option_type(inner_type: &str) -> String {
    format!("Option<{inner_type}>")
}

/// Build a `builtin::array` type name from element type name.
///
/// Examples:
/// - `mangle_builtin_array_type("i32")` → `"builtin::array<i32>"`
pub fn mangle_builtin_array_type(elem_type: &str) -> String {
    format!("builtin::array<{elem_type}>")
}

/// Build a local method name from struct name and method name.
///
/// Examples:
/// - `mangle_local_method("Point", "sum")` → `"Point::sum"`
pub fn mangle_local_method(struct_name: &str, method_name: &str) -> String {
    format!("{struct_name}::{method_name}")
}

/// Build a local method name with trait from struct name, trait name, and method name.
///
/// Examples:
/// - `mangle_local_trait_method("Point", "Display", "fmt")` → `"Point^Display::fmt"`
pub fn mangle_local_trait_method(struct_name: &str, trait_name: &str, method_name: &str) -> String {
    format!("{struct_name}^{trait_name}::{method_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_method_name_to_string_simple() {
        let method = MethodName::new(
            ModuleSource::Local {
                path: "./geometry.wado".to_string(),
            },
            "Point".to_string(),
            None,
            "sum".to_string(),
        );
        assert_eq!(method.to_string(), "./geometry.wado/Point::sum");
    }

    #[test]
    fn test_method_name_to_string_with_trait() {
        let method = MethodName::new(
            ModuleSource::Local {
                path: "./geometry.wado".to_string(),
            },
            "Point".to_string(),
            Some("Display".to_string()),
            "fmt".to_string(),
        );
        assert_eq!(method.to_string(), "./geometry.wado/Point^Display::fmt");
    }

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

    #[test]
    fn test_struct_name_to_string() {
        let struct_name = StructName::from_path_and_name(&["./geometry.wado".to_string()], "Point");
        assert_eq!(struct_name.to_string(), "./geometry.wado/Point");
    }

    #[test]
    fn test_struct_name_from_strs() {
        let struct_name = StructName::from_strs(&["core", "internal"], "SomeType");
        assert_eq!(struct_name.to_string(), "core:internal/SomeType");
    }

    #[test]
    fn test_struct_name_empty_path() {
        let struct_name = StructName::from_path_and_name(&[], "Point");
        assert_eq!(struct_name.to_string(), "<entry>/Point");
    }

    #[test]
    fn test_struct_name_hash_eq() {
        use crate::hashmap::IndexSet;
        let s1 = StructName::from_path_and_name(&["./geometry.wado".to_string()], "Point");
        let s2 = StructName::from_path_and_name(&["./geometry.wado".to_string()], "Point");
        let s3 = StructName::from_path_and_name(&["./other.wado".to_string()], "Point");

        let mut set = IndexSet::default();
        set.insert(s1);
        assert!(set.contains(&s2));
        assert!(!set.contains(&s3));
    }

    #[test]
    fn test_build_core_internal_name() {
        let name = build_core_internal_name("log_stdout");
        assert_eq!(name.to_string(), "core/internal/log_stdout");
        assert_eq!(name.module_source, ModuleSource::internal());
        assert_eq!(name.name, "log_stdout");
    }

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
        // Legacy: empty path represents entry module
        let source = ModuleSource::from_path(&[]);
        assert!(source.is_entry_point());
    }

    #[test]
    fn test_module_source_to_path() {
        let source = ModuleSource::prelude();
        assert_eq!(source.to_path(), vec!["core", "prelude"]);

        let source = ModuleSource::wasi("cli");
        assert_eq!(source.to_path(), vec!["wasi", "cli"]);

        let source = ModuleSource::local("./geometry.wado");
        assert_eq!(source.to_path(), vec!["./geometry.wado"]);

        let source = ModuleSource::entry_point_with_filename("test.wado");
        assert_eq!(source.to_path(), vec!["test.wado"]);
    }

    #[test]
    fn test_module_source_display() {
        assert_eq!(ModuleSource::prelude().to_string(), "core:prelude");
        assert_eq!(ModuleSource::cli().to_string(), "core:cli");
        assert_eq!(ModuleSource::wasi("cli").to_string(), "wasi:cli");
        assert_eq!(
            ModuleSource::local("./geometry.wado").to_string(),
            "./geometry.wado"
        );
        assert_eq!(
            ModuleSource::entry_point_with_filename("hello.wado").to_string(),
            "hello.wado"
        );
    }

    #[test]
    fn test_module_source_helpers() {
        let core = ModuleSource::internal();
        assert!(core.is_core());
        assert!(core.is_core_internal());
        assert!(!core.is_wasi());
        assert!(!core.is_local());

        let builtin = ModuleSource::builtin();
        assert!(builtin.is_core_builtin());

        let prelude = ModuleSource::prelude();
        assert!(prelude.is_core_prelude());

        let wasi = ModuleSource::wasi("cli");
        assert!(wasi.is_wasi());
        assert!(!wasi.is_core());

        let local = ModuleSource::local("./file.wado");
        assert!(local.is_local());
        assert!(!local.is_core());
    }

    #[test]
    fn test_module_source_qualify_name() {
        assert_eq!(
            ModuleSource::prelude().qualify_name("Option"),
            "core:prelude//Option"
        );
        assert_eq!(
            ModuleSource::local("./geometry.wado").qualify_name("Point"),
            "./geometry.wado//Point"
        );
        assert_eq!(
            ModuleSource::wasi("cli").qualify_name("Stdout"),
            "wasi:cli//Stdout"
        );
        assert_eq!(
            ModuleSource::entry_point_with_filename("main.wado").qualify_name("Foo"),
            "main.wado//Foo"
        );
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

    #[test]
    fn test_mangle_ref_aware() {
        assert_eq!(mangle_ref_aware("&", &["i32".into()]), "&i32");
        assert_eq!(mangle_ref_aware("&", &["Array<i32>".into()]), "&Array<i32>");
        assert_eq!(mangle_ref_aware("&mut", &["String".into()]), "&mut String");
        assert_eq!(
            mangle_ref_aware("&mut", &["Array<i32>".into()]),
            "&mut Array<i32>"
        );
        // Non-ref base names fall through to mangle_generic_name
        assert_eq!(mangle_ref_aware("Array", &["i32".into()]), "Array<i32>");
    }
}
