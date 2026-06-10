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
//! - Qualified: `{interface_name}::{operation_name}` (e.g., `Stdout::write_via_stream`)
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

use crate::module_source::{ModuleSource, ModuleSourceInterner};
use fluent_uri::UriRef;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Canonical method name of the synthesised `__call` impl on every
/// closure functor struct. Defined as a single constant so the
/// compiler-internal naming convention has one source of truth — the
/// closure planner, the translator, and DCE all reach for this
/// constant instead of writing the literal `"__call"` at each site.
///
/// Unlike stdlib items wired through [`crate::compiler_item`], the
/// `__call` symbol has no Wado-side declaration, so a `CompilerItem`
/// anchor would have nothing to bind to. A `const` is the right shape.
pub const CLOSURE_CALL_METHOD: &str = "__call";

/// Separator between a namespace-import alias and the imported member in the
/// canonical `ns$member` name a `ns::member` reference resolves to. `$` is not
/// a valid Wado identifier character, so the alias is a single `::`-free token
/// that flows through the per-name import maps (`imported_type_sources`,
/// `imported_globals`, the symbol-table imports) like a `use { X as Y }`
/// alias, scoped to the namespace's own module.
pub const NAMESPACE_MEMBER_SEP: char = '$';

/// Build the `ns$member` alias for a namespace-imported symbol.
pub fn namespace_member_alias(namespace: &str, member: &str) -> String {
    format!("{namespace}{NAMESPACE_MEMBER_SEP}{member}")
}

/// Prefix the compiler stamps onto every synthesised closure-functor
/// struct (`__Closure_0`, `__Closure_1`, …). Like
/// [`CLOSURE_CALL_METHOD`], this is purely a compiler-internal
/// convention — there is no Wado-side declaration to anchor it to.
pub const CLOSURE_STRUCT_PREFIX: &str = "__Closure_";

/// Canonical name of the synthesised closure-trait family (`Fn<N, Ret>`).
/// Like [`CLOSURE_CALL_METHOD`] / [`CLOSURE_STRUCT_PREFIX`], the trait is
/// compiler-internal — there is no Wado-side `trait Fn { ... }` declaration
/// to attach a `#[compiler_item("...")]` to — so a `const` is the right
/// anchor shape.
pub const CLOSURE_FN_TRAIT: &str = "Fn";

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
    /// Base generic name if monomorphized (e.g., "List" for "List<i32>`::len`")
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
    pub fn from_path_and_name(
        interner: &mut ModuleSourceInterner,
        module_path: &[String],
        name: &str,
    ) -> Self {
        Self {
            module_source: interner.from_path(module_path),
            name: name.to_string(),
            is_monomorphized: false,
            base_name: None,
        }
    }

    /// Create a `FreeFunctionName` from string literal slices.
    /// Convenience method for when you have &[&str] instead of &[String].
    pub fn from_strs(
        interner: &mut ModuleSourceInterner,
        module_path: &[&str],
        name: &str,
    ) -> Self {
        let path: Vec<String> = module_path.iter().map(|s| (*s).to_string()).collect();
        Self {
            module_source: interner.from_path(&path),
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
            ModuleSource::Redirected { uri } => write!(f, "{}/{}", uri, self.name),
            ModuleSource::Wasm { path, .. } => write!(f, "{}/{}", path, self.name),
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
    /// The trait/effect/resource name, possibly with type args
    /// (e.g., `"Display"`, `"Stream<u8>"`).
    pub trait_name: Option<String>,
    /// The base trait name without type args (e.g., "Display", "Stream").
    /// Mirrors `base_struct_name`: kept alongside `trait_name` so generic
    /// trait/resource impls (`impl Stream<u8> for MockCM`) round-trip a
    /// distinct mangled `trait_name` for codegen while still resolving
    /// against the bare-name decl indices used by trait/effect/resource
    /// dispatch.
    pub base_trait_name: Option<String>,
    /// Canonical declaring module of the trait this method implements.
    /// Populated by the elaborator via
    /// [`crate::elaborator::Elaborator::canonical_decl_key`] when constructing
    /// the method from an `impl <Trait> for <Type>` block. Left `None` for
    /// synthesis-derived auto-impls (Inspect / Display / Eq / Ord / From
    /// / `serde` adapters, …) because those impls target prelude / core
    /// traits whose name is already project-globally unique — dispatch
    /// synthesis identifies them by name alone and never needs the
    /// disambiguating module.
    ///
    /// When `Some`, paired with `base_trait_name` it forms the canonical
    /// `(module, name)` key into [`crate::elaborator::trait_env::EffectDeclIndex`]
    /// and [`crate::elaborator::trait_env::ResourceDeclIndex`]. Two modules
    /// can each declare `pub interface Logger`; without this field the
    /// dispatch builder would collapse both impls onto whichever Logger
    /// landed first in the bare-name lookup table.
    pub base_trait_module: Option<ModuleSource>,
    /// Concrete `TypeId`s of the trait / resource type arguments at this
    /// impl site (e.g. `[u8]` for `impl Stream<u8> for MockCM`). Empty
    /// for non-generic traits / effects, and for the bare base form
    /// recorded outside of impl-block method context. The dispatch
    /// synthesis consumes this to produce **per-monomorphisation**
    /// dispatch infrastructure: each unique `(base_trait, trait_type_args)`
    /// pair gets its own `__Dispatch_<R>__<args>` struct + global +
    /// per-op wrappers, with the resource's operation types substituted
    /// for that combination.
    pub trait_type_args: Vec<crate::tir::TypeId>,
    /// The method name (e.g., "sum" or "fmt")
    pub method_name: String,
    /// Method-level type args (e.g., ["i64"] for transform<i64>)
    pub method_type_args: Vec<String>,
    /// Whether the struct name is a type parameter that should be substituted directly
    /// during monomorphization (e.g., `T^Ord::cmp` where T should become i32).
    pub is_type_param_receiver: bool,
    /// Whether this method is from an `impl Trait for &T` or `impl Trait for &mut T`.
    /// When true, the function name uses the inner type name (e.g., "List") but the
    /// actual impl is on the reference type (e.g., &List<T>).
    pub is_ref_impl: bool,
    /// CM canonical name from `#[cm("...")]` attribute on resource methods.
    /// When set, synthesis generates a CM binding function and rewrites
    /// the call site to use it instead of the original resource method.
    pub cm_name: Option<String>,
}

/// Derive the bare base name from a possibly-mangled type/trait name.
///
/// `mangle_ref_aware` and friends produce names like `Stream<u8>` or
/// `From<i32>` by appending the type-arg list to the base name; the
/// reverse — recovering the base by truncating at the first `<` — is
/// the canonical inverse and lives here so other components stay
/// agnostic to name-format details (per the wado-compiler CLAUDE
/// rules: "Use utilities in name.rs to handle name mangling and
/// monomorphization. Other components must not know the details of
/// name formats.").
fn split_base_name(name: &str) -> &str {
    match name.find('<') {
        Some(i) => &name[..i],
        None => name,
    }
}

impl LocalMethodName {
    /// Create a new `LocalMethodName` directly from components.
    ///
    /// IMPORTANT: `struct_name` must be the base struct name WITHOUT type parameters.
    /// Use `with_type_args()` or `with_struct_type_args()` to add type parameters.
    ///
    /// `trait_name` may be either the bare base form (`"Display"`) or a
    /// pre-mangled form (`"Stream<u8>"`); `base_trait_name` is derived by
    /// truncating at the first `<`. `base_trait_module` is left `None` and
    /// callers that have the canonical declaring module (elaborator path)
    /// should populate it via [`Self::with_base_trait_module`]; synthesis-
    /// derived auto-impls leave it `None` because dispatch synthesis
    /// identifies them by name alone.
    #[must_use]
    pub fn new(struct_name: String, trait_name: Option<String>, method_name: String) -> Self {
        debug_assert!(
            !struct_name.contains('<'),
            "LocalMethodName::new() expects base struct name without type params, got: {struct_name}"
        );
        let base_trait_name = trait_name
            .as_deref()
            .map(|n| split_base_name(n).to_string());
        Self {
            base_struct_name: struct_name.clone(),
            struct_name,
            base_trait_name,
            base_trait_module: None,
            trait_name,
            trait_type_args: Vec::new(),
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
    /// `trait_name` may be either bare or pre-mangled — see `new` for the
    /// derivation rule for `base_trait_name`.
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
        let base_trait_name = trait_name
            .as_deref()
            .map(|n| split_base_name(n).to_string());
        Self {
            base_struct_name: struct_name.clone(),
            struct_name,
            base_trait_name,
            base_trait_module: None,
            trait_name,
            trait_type_args: Vec::new(),
            method_name,
            method_type_args,
            is_type_param_receiver: false,
            is_ref_impl: false,
            cm_name: None,
        }
    }

    /// Attach the canonical declaring module of `base_trait_name`. Used by
    /// the elaborator path that lifts an `impl <Trait> for <Type>` block into
    /// TIR: the trait reference is canonicalised through
    /// [`crate::elaborator::Elaborator::canonical_decl_key`] and then threaded
    /// into the per-method `LocalMethodName` so dispatch synthesis can
    /// distinguish two modules' same-named effects / resources.
    #[must_use]
    pub fn with_base_trait_module(mut self, module: Option<ModuleSource>) -> Self {
        self.base_trait_module = module;
        self
    }

    /// Create a version of this `LocalMethodName` with type args applied.
    ///
    /// `impl_type_args` are applied to the struct name (e.g., "List" + ["i32"] → "List<i32>").
    /// `method_type_args` are stored separately (not embedded in `method_name`).
    /// `base_struct_name`, `base_trait_name`, and `base_trait_module` are
    /// preserved (not changed by type args).
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
            base_trait_name: self.base_trait_name.clone(),
            base_trait_module: self.base_trait_module.clone(),
            trait_type_args: self.trait_type_args.clone(),
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

    /// Create a version with the trait name mangled with type args.
    ///
    /// `trait_type_args` are applied to the trait name (e.g.,
    /// `"Stream"` + `["u8"]` → `"Stream<u8>"`). `base_trait_name` and
    /// `base_trait_module` are preserved so dispatch synthesis / decl-
    /// index lookups continue to resolve against the bare trait
    /// declaration (and its module).
    ///
    /// Panics if `self.trait_name` is `None` — type args on an inherent
    /// method don't have a trait to mangle.
    #[must_use]
    pub fn with_trait_type_args(&self, trait_type_args: &[String]) -> Self {
        let base = self
            .base_trait_name
            .clone()
            .expect("with_trait_type_args() requires a trait name");
        let mangled = if trait_type_args.is_empty() {
            base.clone()
        } else {
            mangle_ref_aware(&base, trait_type_args)
        };
        Self {
            trait_name: Some(mangled),
            base_trait_name: Some(base),
            ..self.clone()
        }
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
            base_trait_name: self.base_trait_name.clone(),
            base_trait_module: self.base_trait_module.clone(),
            trait_type_args: self.trait_type_args.clone(),
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

    /// Returns true if this is the synthesized `__call` method on a
    /// `__Closure_N` functor struct.
    ///
    /// Closure functor `__call` methods are inherent methods syntactically
    /// (`trait_name` is `None`), but they participate in vtable dispatch
    /// through the `Fn<arity, ret>` canonical type whose Wasm signature is
    /// fixed. Treating them as ordinary inherent methods (e.g. for ABI
    /// reshaping like multi-value return) would skew the signature against
    /// the vtable slot they're installed into, so callers that reshape
    /// ABIs need to filter them out.
    pub fn is_closure_call(&self) -> bool {
        self.method_name == CLOSURE_CALL_METHOD
            && self.struct_name.starts_with(CLOSURE_STRUCT_PREFIX)
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
    pub fn from_path_and_name(
        interner: &mut ModuleSourceInterner,
        module_path: &[String],
        name: &str,
    ) -> Self {
        Self {
            module_source: interner.from_path(module_path),
            name: name.to_string(),
        }
    }

    /// Create a `StructName` from string slices.
    /// This is a convenience method for tests and initialization.
    #[must_use]
    pub fn from_strs(
        interner: &mut ModuleSourceInterner,
        module_path: &[&str],
        name: &str,
    ) -> Self {
        let path: Vec<String> = module_path.iter().map(|&s| s.to_string()).collect();
        Self {
            module_source: interner.from_path(&path),
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
pub fn build_core_internal_name(
    interner: &mut ModuleSourceInterner,
    name: &str,
) -> FreeFunctionName {
    FreeFunctionName::from_strs(interner, &["core", "internal"], name)
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
pub fn resolve_import(
    interner: &mut ModuleSourceInterner,
    from_module: &ModuleSource,
    import_source: &str,
) -> ModuleSource {
    resolve_import_with_entry(interner, from_module, import_source, None)
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
    interner: &mut ModuleSourceInterner,
    from_module: &ModuleSource,
    import_source: &str,
    entry_module: Option<&ModuleSource>,
    invocations: &crate::kiln::InvocationIndex,
) -> ModuleSource {
    if !invocations.is_empty() {
        let decl_file = match from_module {
            ModuleSource::Local { path } => path.as_str(),
            ModuleSource::EntryPoint { filename } => filename.as_str(),
            ModuleSource::Redirected { uri } => uri.as_str(),
            _ => "",
        };
        if let Some(entry_uri) = invocations.redirect(decl_file, import_source) {
            return interner.redirected(entry_uri);
        }
    }
    resolve_import_with_entry(interner, from_module, import_source, entry_module)
}

pub fn resolve_import_with_entry(
    interner: &mut ModuleSourceInterner,
    from_module: &ModuleSource,
    import_source: &str,
    entry_module: Option<&ModuleSource>,
) -> ModuleSource {
    // Handle special prefixes
    if let Some(name) = import_source.strip_prefix("core:") {
        return interner.core(name);
    }
    if let Some(interface) = import_source.strip_prefix("wasi:") {
        return interner.wasi(interface);
    }
    if import_source.starts_with("https://") || import_source.starts_with("http://") {
        return interner.remote(import_source);
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
        return interner.local(&resolved);
    }

    // Fallback: normalize and return as Local path
    // This handles EntryPoint imports and bare imports
    interner.local(&normalize_module_path(import_source))
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
    /// `Array<T>` (raw Wasm GC array, NOT the user-facing `List<T>` struct)
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
        TypeNameInfo::Unit => "()".to_string(),
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
/// For `&` and `&mut` base names, formats as prefix: `&List<i32>`, `&mut List<i32>`.
/// For other base names, formats as generic: `List<i32>`, `Map<String,i32>`.
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

/// Rewrite a mangled generic type name so every type argument uses its
/// simple (unqualified) name: `List<core:prelude/string.wado/String>` →
/// `List<String>`. The head name is left untouched.
///
/// A concrete `impl Trait for List<String>` block names its methods with
/// the AST's simple argument spelling, while monomorphized call sites
/// qualify arguments (`mangle_type_name`); this bridges the two spellings
/// for lookups. Names without type arguments pass through unchanged.
pub fn simplify_generic_type_args(mangled: &str) -> String {
    let Some(lt) = mangled.find('<') else {
        return mangled.to_string();
    };
    let (head, args) = mangled.split_at(lt);
    let mut out = String::with_capacity(mangled.len());
    out.push_str(head);
    let mut segment_start = out.len();
    for c in args.chars() {
        match c {
            '<' | ',' | '>' => {
                out.push(c);
                segment_start = out.len();
            }
            // Drop the qualifier accumulated since the last delimiter;
            // repeated separators (`core:prelude/string.wado/String`)
            // strip stepwise until only the simple name remains.
            '/' => out.truncate(segment_start),
            _ => out.push(c),
        }
    }
    out
}

/// Build a monomorphized method name from struct name, type args, and method name.
///
/// Examples:
/// - `mangle_method_generic("Box", &["i32"], "get")` → `"Box<i32>::get"`
/// - `mangle_method_generic("List", &["String"], "len")` → `"List<String>::len"`
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

/// Build the raw GC array type name from element type name.
///
/// Examples:
/// - `mangle_builtin_array_type("i32")` → `"Array<i32>"`
pub fn mangle_builtin_array_type(elem_type: &str) -> String {
    format!("Array<{elem_type}>")
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

/// Build the per-instantiation effect-dispatch struct name.
///
/// `label` is the dispatch instantiation label produced by the
/// effect-dispatch synthesis (`Counter`, `Stream<u8>`, …).
///
/// Examples:
/// - `dispatch_struct_name("Counter")` → `"__Dispatch_Counter"`
/// - `dispatch_struct_name("Stream<u8>")` → `"__Dispatch_Stream<u8>"`
pub fn dispatch_struct_name(label: &str) -> String {
    format!("__Dispatch_{label}")
}

/// Build the per-instantiation effect-dispatch global name.
///
/// Examples:
/// - `dispatch_global_name("Counter")` → `"__effect_Counter"`
/// - `dispatch_global_name("Stream<u8>")` → `"__effect_Stream<u8>"`
pub fn dispatch_global_name(label: &str) -> String {
    format!("__effect_{label}")
}

/// Build the per-operation effect-dispatch wrapper function name.
///
/// Examples:
/// - `dispatch_wrapper_name("Counter", "next")` → `"__effect_dispatch__Counter__next"`
/// - `dispatch_wrapper_name("Stream<u8>", "read")` → `"__effect_dispatch__Stream<u8>__read"`
pub fn dispatch_wrapper_name(label: &str, op_name: &str) -> String {
    format!("__effect_dispatch__{label}__{op_name}")
}

/// Build the dispatch struct's per-operation field name.
///
/// Examples:
/// - `dispatch_field_name("next")` → `"op_next"`
/// - `dispatch_field_name("read")` → `"op_read"`
pub fn dispatch_field_name(op_name: &str) -> String {
    format!("op_{op_name}")
}

/// Convert a user-facing `test "name"` string into the snake-case segment used
/// in the internal test function name (`__test_{index}_{snake}`).
///
/// Only **ASCII** alphanumerics survive verbatim (lowercased); every other
/// character — including non-ASCII letters such as `é` or `日` — collapses to
/// `_`. This is deliberate: the segment must downgrade losslessly into a
/// Component Model kebab-case export name (`[a-z0-9-]+`) via
/// `sanitize_kebab_export_name`. Using Unicode-aware `char::is_alphanumeric`
/// here would let multibyte letters through and produce an invalid extern name,
/// crashing Wasm validation. The original (lossless) name is preserved
/// separately for display and filtering — see the test-name custom section.
///
/// Examples:
/// - `test_name_to_snake("Hello, World!")` → `"hello__world_"`
/// - `test_name_to_snake("café résumé")` → `"caf__r_sum_"`
/// - `test_name_to_snake("日本語のテスト ok")` → `"________ok"`
pub fn test_name_to_snake(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_name_to_snake_is_ascii_only() {
        // ASCII alphanumerics survive (lowercased); punctuation/space → `_`.
        assert_eq!(test_name_to_snake("Hello, World!"), "hello__world_");
        // Non-ASCII letters collapse to `_` so the kebab export stays valid.
        assert_eq!(test_name_to_snake("café résumé"), "caf__r_sum_");
        assert_eq!(test_name_to_snake("日本語のテスト ok"), "________ok");
        // A fully non-ASCII name leaves no ASCII segment at all.
        assert_eq!(test_name_to_snake("完全に日本語"), "______");
        // The output must never contain a non-ASCII byte.
        for input in ["café résumé", "日本語のテスト ok", "Ω±∞ μ"] {
            assert!(
                test_name_to_snake(input).is_ascii(),
                "snake form of {input:?} must be ASCII"
            );
        }
    }

    #[test]
    fn test_method_name_to_string_simple() {
        let mut interner = ModuleSourceInterner::new();
        let method = MethodName::new(
            interner.local("./geometry.wado"),
            "Point".to_string(),
            None,
            "sum".to_string(),
        );
        assert_eq!(method.to_string(), "./geometry.wado/Point::sum");
    }

    #[test]
    fn test_method_name_to_string_with_trait() {
        let mut interner = ModuleSourceInterner::new();
        let method = MethodName::new(
            interner.local("./geometry.wado"),
            "Point".to_string(),
            Some("Display".to_string()),
            "fmt".to_string(),
        );
        assert_eq!(method.to_string(), "./geometry.wado/Point^Display::fmt");
    }

    #[test]
    fn test_free_function_name_to_string() {
        let mut interner = ModuleSourceInterner::new();
        let func = FreeFunctionName::from_path_and_name(
            &mut interner,
            &["core".to_string(), "cli".to_string()],
            "println",
        );
        assert_eq!(func.to_string(), "core/cli/println");
    }

    #[test]
    fn test_free_function_name_from_strs() {
        let mut interner = ModuleSourceInterner::new();
        let func = FreeFunctionName::from_strs(&mut interner, &["core", "internal"], "log_stdout");
        assert_eq!(func.to_string(), "core/internal/log_stdout");
    }

    #[test]
    fn test_free_function_name_empty_path() {
        let mut interner = ModuleSourceInterner::new();
        let func = FreeFunctionName::from_strs(&mut interner, &[], "main");
        assert_eq!(func.to_string(), "main");
    }

    #[test]
    fn test_struct_name_to_string() {
        let mut interner = ModuleSourceInterner::new();
        let struct_name = StructName::from_path_and_name(
            &mut interner,
            &["./geometry.wado".to_string()],
            "Point",
        );
        assert_eq!(struct_name.to_string(), "./geometry.wado/Point");
    }

    #[test]
    fn test_struct_name_from_strs() {
        let mut interner = ModuleSourceInterner::new();
        let struct_name = StructName::from_strs(&mut interner, &["core", "internal"], "SomeType");
        assert_eq!(struct_name.to_string(), "core:internal/SomeType");
    }

    #[test]
    fn test_struct_name_empty_path() {
        let mut interner = ModuleSourceInterner::new();
        let struct_name = StructName::from_path_and_name(&mut interner, &[], "Point");
        assert_eq!(struct_name.to_string(), "<entry>/Point");
    }

    #[test]
    fn test_struct_name_hash_eq() {
        use crate::hashmap::IndexSet;
        let mut interner = ModuleSourceInterner::new();
        let s1 = StructName::from_path_and_name(
            &mut interner,
            &["./geometry.wado".to_string()],
            "Point",
        );
        let s2 = StructName::from_path_and_name(
            &mut interner,
            &["./geometry.wado".to_string()],
            "Point",
        );
        let s3 =
            StructName::from_path_and_name(&mut interner, &["./other.wado".to_string()], "Point");

        let mut set = IndexSet::default();
        set.insert(s1);
        assert!(set.contains(&s2));
        assert!(!set.contains(&s3));
    }

    #[test]
    fn test_build_core_internal_name() {
        let mut interner = ModuleSourceInterner::new();
        let name = build_core_internal_name(&mut interner, "log_stdout");
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
    fn test_mangle_ref_aware() {
        assert_eq!(mangle_ref_aware("&", &["i32".into()]), "&i32");
        assert_eq!(mangle_ref_aware("&", &["List<i32>".into()]), "&List<i32>");
        assert_eq!(mangle_ref_aware("&mut", &["String".into()]), "&mut String");
        assert_eq!(
            mangle_ref_aware("&mut", &["List<i32>".into()]),
            "&mut List<i32>"
        );
        // Non-ref base names fall through to mangle_generic_name
        assert_eq!(mangle_ref_aware("List", &["i32".into()]), "List<i32>");
    }
}
