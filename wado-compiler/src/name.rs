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
//! Module paths are filesystem representations, not URIs: they are canonicalized
//! by lexical normalization (`crate::path::normalize`, RFC 3986 §5.2.4
//! dot-segment semantics) — never percent-encoded — to ensure:
//! - Same file imported via different paths resolves to same identity
//! - Always uses `/` separator (platform-agnostic, even on Windows)
//! - Resolves `.` and `..` segments
//!
//! Canonical paths are project-root-relative:
//! - For projects with `wado.toml`: relative to the directory containing `wado.toml`
//! - For standalone scripts: relative to the entry point's directory

use crate::module_source::{ModuleSource, ModuleSourceInterner};
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

/// Separator between a local item's declared name and its disambiguating
/// `AstId` in the internal storage name a function-scoped `struct`/`type`
/// declaration (`Stmt::Item`) is minted under (see the local-item-definitions
/// WEP). `@` is not a valid Wado identifier character. This is the type's
/// *storage identity*, used to key the durable, module-wide field/case
/// tables so two sibling functions' same-named local items never collide;
/// [`crate::tir::TypeTable::type_name`] strips it back off before the name
/// reaches a diagnostic.
pub const LOCAL_ITEM_ID_SEP: char = '@';

/// Build the internal storage name for a local item declaration: the declared
/// name plus the `AstId`'s module-local index, unique per declaration site so
/// same-named local items in different functions never collide in the
/// module-wide storage tables they're interned into.
///
/// Only the `local` index is encoded — never the `AstIdSpace`. The space is a
/// process-global counter whose value depends on unrelated parse history
/// (e.g. how many fixtures a parallel golden run compiled first), so encoding
/// it would leak into mangled WIR names and make compiler output
/// non-deterministic. The `local` index is dense per module and the module
/// source already qualifies the name across modules, so it alone suffices.
pub fn mangle_local_item_name(name: &str, id: crate::ast::AstId) -> String {
    format!("{name}{LOCAL_ITEM_ID_SEP}{}", id.local())
}

/// Recover a local item's user-declared name from its internal storage name
/// (see [`mangle_local_item_name`]). Returns `name` unchanged if it isn't a
/// local-item storage name (no [`LOCAL_ITEM_ID_SEP`]).
pub fn strip_local_item_id(name: &str) -> &str {
    name.split(LOCAL_ITEM_ID_SEP).next().unwrap_or(name)
}

/// Replace every occurrence of the type name `old` inside a mangled function
/// name (`List<Old>::with_capacity`, `Map<Old, Other>.insert`) with `new`,
/// matching only at type-name boundaries. A raw substring replace would also
/// rewrite names that merely contain `old` as a fragment (`Old` inside
/// `OldExtended`); a boundary match cannot.
pub fn replace_type_name_in_mangled(mangled: &str, old: &str, new: &str) -> String {
    fn is_boundary(c: Option<char>) -> bool {
        match c {
            None => true,
            Some(c) => !(c.is_alphanumeric() || c == '_'),
        }
    }
    let mut out = String::with_capacity(mangled.len());
    let mut rest = mangled;
    while let Some(pos) = rest.find(old) {
        out.push_str(&rest[..pos]);
        // Left context comes from everything emitted so far — `rest` alone
        // loses it once a previous match consumed the preceding characters.
        let before = out.chars().next_back();
        let after = rest[pos + old.len()..].chars().next();
        if is_boundary(before) && is_boundary(after) {
            out.push_str(new);
        } else {
            out.push_str(old);
        }
        rest = &rest[pos + old.len()..];
    }
    out.push_str(rest);
    out
}

/// The name of the synthesized deep-copy helper for a value type, identified
/// by its module-qualified structural mangle
/// (`TypeTable::mangle_type_arg_for_generic`). The mangle is a stable,
/// path-independent identity — unlike an intern-order `TypeId` index — so the
/// same helper carries the same name in the WIR dump, the codegen output, and
/// the wasm name section, and identical types that were interned more than
/// once collapse onto one helper.
pub fn value_copy_helper_name(mangled_type: &str) -> String {
    format!("$value_copy${mangled_type}")
}

/// The name of the shallow-copy sibling `value_copy_demote` synthesizes for a
/// deep value-copy helper: the deep helper's name with a `$shallow` suffix.
pub fn shallow_copy_helper_name(deep_name: &str) -> String {
    format!("{deep_name}$shallow")
}

/// The name of the synthesized `Case::<V, P>::extract` helper for a variant
/// and one of its payload types, both identified by their structural mangles
/// (same identity discipline as [`value_copy_helper_name`]). Lowering rewrites
/// `builtin::variant_case_extract::<V, P>` calls to it (WEP 2026-06-13 §3e).
pub fn case_extract_helper_name(mangled_variant: &str, mangled_payload: &str) -> String {
    format!("$case_extract${mangled_variant}${mangled_payload}")
}

/// The `Case::<V, P>::construct` sibling of [`case_extract_helper_name`].
pub fn case_construct_helper_name(mangled_variant: &str, mangled_payload: &str) -> String {
    format!("$case_construct${mangled_variant}${mangled_payload}")
}

/// The `discriminant` of one instantiated generic variant, identified by the
/// instance's structural mangle. A free helper like the value bridges: the
/// method-name form cannot spell an instance.
pub fn variant_tag_helper_name(mangled_variant: &str) -> String {
    format!("$variant_tag${mangled_variant}")
}

/// The name of the synthesized `StructField::<S, F>::get` helper for a struct and one
/// of its field types, both identified by their structural mangles (same
/// identity discipline as [`case_extract_helper_name`]). Lowering rewrites
/// `builtin::struct_field_get::<S, F>` calls to it (WEP 2026-06-13 §2).
pub fn field_get_helper_name(mangled_struct: &str, mangled_field: &str) -> String {
    format!("$field_get${mangled_struct}${mangled_field}")
}

/// Field name of the discriminant slot on a variant's base struct (the tag
/// every case subtype inherits). Single source of truth for the WIR variant
/// GC representation: the builder (`wir_build/types.rs`, `pattern_match.rs`,
/// `translate.rs`), the multi-value SROA (`sroa_variant_return.rs`), the codegen
/// field map (`codegen/emit.rs`), and the `nullable_ref` lowering all key on
/// this exact name, so a literal drift would produce invalid Wasm. Builder-side
/// call sites should adopt this constant (and [`variant_payload_field`] /
/// [`is_variant_payload_field`]) so the convention cannot silently diverge.
pub const VARIANT_DISCRIMINANT_FIELD: &str = "discriminant";

/// Field name of the `index`-th payload slot on a variant case subtype
/// (`payload_0`, `payload_1`, ...). Paired with [`VARIANT_DISCRIMINANT_FIELD`]
/// as the canonical variant-field naming; see that constant's note.
pub fn variant_payload_field(index: usize) -> String {
    format!("payload_{index}")
}

/// Whether `field_name` is a variant case payload slot (see
/// [`variant_payload_field`]). Matches the `payload_` prefix; the multi-value
/// SROA's `case{N}_payload_{M}` slots carry a `case` prefix and so are
/// deliberately not matched.
pub fn is_variant_payload_field(field_name: &str) -> bool {
    field_name.starts_with("payload_")
}

/// Convert a Wado identifier (`snake_case` / `PascalCase` / `camelCase`) to
/// Component Model kebab-case (`my-api`, `http-server`, `error-code`).
///
/// Public so out-of-crate boundary code (e.g. the CLI host materializing a
/// generator's options from canonical CBOR) can map raw Wado identifiers to the
/// CM names a component exposes, instead of re-deriving the format.
pub fn to_kebab(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower_or_digit = false;
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '_' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            prev_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() {
            // Break before an uppercase letter that starts a new word: either
            // after a lowercase/digit (`myApi` -> `my-api`), or at the end of
            // an acronym run when the next char is lowercase
            // (`HTTPServer` -> `http-server`).
            let acronym_boundary = chars.get(i + 1).is_some_and(char::is_ascii_lowercase)
                && i > 0
                && chars[i - 1].is_ascii_uppercase();
            if (prev_lower_or_digit || acronym_boundary) && !out.is_empty() {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower_or_digit = false;
        } else {
            out.push(ch);
            prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    out
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

/// Label the template-string synthesiser stamps on the block wrapping an
/// expanded `` `...` `` literal. The template-hoist and const-branch-prune
/// optimizers key on it to recognise template expansions, so the producer
/// (`synthesis::template`) and those consumers share this one definition
/// instead of re-hardcoding the literal — compiler-internal, hence a `const`.
pub const TEMPLATE_BLOCK_LABEL: &str = "__tmpl";

/// Name of the result accumulator local in an expanded template block.
/// Recognised by the template-hoist optimizer; single-sourced here.
pub const TEMPLATE_RESULT_LOCAL: &str = "__r";

/// Name of the `Formatter` local in an expanded template block. Producer-only
/// today, kept beside its siblings so the template-local convention lives in
/// one place.
pub const TEMPLATE_FORMATTER_LOCAL: &str = "__f";

/// Per-module initializer function the lowering phase synthesises to run a
/// module's global initializers. The optimizer's liveness / const-object
/// passes treat it as a root, so producer and consumers share this name.
pub const MODULE_INIT_FUNCTION: &str = "__initialize_module";

/// Aggregate initializer that calls every module's [`MODULE_INIT_FUNCTION`].
/// Shares the [`MODULE_INIT_FUNCTION`] prefix, so a `starts_with`
/// over the latter still covers both.
pub const MODULES_INIT_FUNCTION: &str = "__initialize_modules";

/// Prefix the const-object globalization pass stamps on the globals it hoists
/// constant aggregates into (`__const_obj_0`, …). It both mints and rescans
/// these names, so the prefix lives here rather than as a repeated literal.
pub const CONST_OBJ_GLOBAL_PREFIX: &str = "__const_obj_";

/// Maximum UTF-8 byte length for an `InlineRef`-hoisted global (see
/// [`crate::nir::NirGlobal::prefer_fixed_string_repr`]) to override the
/// package-wide `string_inline_max_bytes` threshold and materialize as a
/// constant `array.new_fixed<u8>`. Bounded so a large hoisted literal still
/// falls back to compact `array.new_data` instead of bloating `-Os` builds.
pub const INLINE_REF_EAGER_MAX_BYTES: usize = 64;

/// A free function name (not a method on a struct).
///
/// Format: `{module_source}/{name}`
///
/// Examples:
/// - `./geometry.wado/helper`
/// - `core/rt/log_stdout`
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
        // Every module is qualified the same way through its portable path;
        // the entry point contributes its base name like any other module.
        write!(f, "{}/{}", self.module_source.to_path_string(), self.name)
    }
}

/// A method name on a struct.
///
/// Format:
/// - Without trait: `{struct_name}::{method_name}`
/// - With trait: `{struct_name}^{trait_name}::{method_name}`
///
/// Examples:
/// - `./geometry.wado/Point::sum`
/// - `./geometry.wado/Point^Display::fmt`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodName {
    /// The module source where the method is defined
    pub module_source: ModuleSource,
    /// The receiver this method hangs off (e.g., `./geom.wado/Point`).
    pub struct_name: FqTypeName,
    /// The trait name if this is a trait implementation (e.g., `Display`)
    pub trait_name: Option<String>,
    /// The method name (e.g., `sum`)
    pub method_name: String,
}

impl MethodName {
    pub fn new(
        module_source: ModuleSource,
        struct_name: FqTypeName,
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
    pub fn format_local(
        struct_name: &FqTypeName,
        trait_name: Option<&str>,
        method_name: &str,
    ) -> String {
        match trait_name {
            Some(trait_n) => format!("{struct_name}^{trait_n}::{method_name}"),
            None => format!("{struct_name}::{method_name}"),
        }
    }

    /// Format a base name with type arguments and optional trait.
    /// Format: `Struct<TypeArgs>^Trait` or `Struct<TypeArgs>`.
    ///
    /// `ref_kind` carries the receiver's reference shape so a `&T` / `&mut T`
    /// receiver mangles with a prefix; it comes from the typed receiver, never
    /// from inspecting `base`.
    pub fn format_struct_with_args(
        base: &str,
        ref_kind: Option<RefKind>,
        type_args: &[String],
        trait_name: Option<&str>,
    ) -> String {
        let struct_part = if type_args.is_empty() {
            base.to_string()
        } else {
            Receiver::mangle_with_ref(base, ref_kind, type_args)
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
    /// The receiver already names the module that declares it, so nothing is
    /// prefixed here. [`Self::module_source`] — where the *impl* lives, which
    /// need not be the receiver's module — is printed by whoever needs it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.local_name())
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
    /// The typed receiver shape (`Point`, `&T`, `S::SeqSerializer`). Its
    /// `head_key()` is the base identity string preserved across monomorphization.
    pub receiver: Receiver,
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
    /// The receiver's type arguments, structured. Together with `receiver`
    /// they *are* `struct_name`: [`Self::fq_struct_name`] rebuilds the
    /// instantiated receiver from them rather than reading the rendered
    /// `struct_name` back apart.
    pub struct_type_args: Vec<FqTypeName>,
    /// The method name (e.g., "sum" or "fmt")
    pub method_name: String,
    /// Method-level type args (e.g., ["i64"] for transform<i64>)
    pub method_type_args: Vec<String>,
    /// Whether the struct name is a type parameter that should be substituted directly
    /// during monomorphization (e.g., `T^Ord::cmp` where T should become i32).
    pub is_type_param_receiver: bool,
    /// Whether this method came from an `impl Trait for &T` / `&mut T` block —
    /// a semantic flag (self-adjustment, codegen), distinct from the receiver's
    /// head being `&` (which `receiver` records).
    pub is_ref_impl: bool,
    /// CM canonical name from `#[cm("...")]` attribute on resource methods.
    /// When set, synthesis generates a CM binding function and rewrites
    /// the call site to use it instead of the original resource method.
    pub cm_name: Option<String>,
}

/// Derive the bare base name from a possibly-mangled type/trait name.
///
/// `Receiver::mangle` and friends produce names like `Stream<u8>` or
/// `From<i32>` by appending the type-arg list to the base name; the
/// reverse — recovering the base by truncating at the first `<` — is
/// the canonical inverse and lives here so other components stay
/// agnostic to name-format details (per the wado-compiler CLAUDE
/// rules: "Use utilities in name.rs to handle name mangling and
/// monomorphization. Other components must not know the details of
/// name formats.").
pub(crate) fn split_base_name(name: &str) -> &str {
    match name.find('<') {
        Some(i) => &name[..i],
        None => name,
    }
}

/// Whether a mangled call name denotes a trait-method impl
/// (`Type^Trait::method`); the `^` separates the receiver type from its trait.
pub fn is_local_trait_method_name(name: &str) -> bool {
    name.contains('^')
}

/// The reference kind of a `&` / `&mut` method receiver.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RefKind {
    Shared,
    Mut,
}

impl RefKind {
    /// The mangle prefix — the sole spelling of `&` / `&mut` from a `RefKind`.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            RefKind::Shared => "&",
            RefKind::Mut => "&mut",
        }
    }

    /// The ref kind of an AST type, or `None` for a non-reference.
    #[must_use]
    pub fn from_ast(ty: &crate::ast::Type) -> Option<Self> {
        match ty {
            crate::ast::Type::Reference(_) => Some(RefKind::Shared),
            crate::ast::Type::MutReference(_) => Some(RefKind::Mut),
            _ => None,
        }
    }

    /// The ref kind of a resolved type, or `None` for a non-reference.
    #[must_use]
    pub fn from_resolved(ty: &crate::tir::ResolvedType) -> Option<Self> {
        match ty {
            crate::tir::ResolvedType::Ref(_) => Some(RefKind::Shared),
            crate::tir::ResolvedType::MutRef(_) => Some(RefKind::Mut),
            _ => None,
        }
    }
}

/// The shape of a method receiver, typed so nothing inspects a mangled name to
/// recover it. The `&` / `&mut` / `::` spellings appear only in this type's
/// `mangle` / `head_key`; producers build it from typed AST / resolved types.
///
/// Whether the receiver is a generic type parameter to substitute is an
/// orthogonal concern (a `Projection` head can be a param too), kept as
/// `LocalMethodName::is_type_param_receiver` rather than a variant here.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Receiver {
    /// A named type or type parameter: `Point`, `List`, `T`.
    Type(FqTypeName),
    /// A universal reference receiver `&T` / `&mut T`; the pointee rides in the
    /// receiver's type-arg list, not here.
    Ref(RefKind),
    /// An associated-type projection `Base::Assoc` (`S::SeqSerializer`).
    Projection { base: String, assoc: String },
}

impl Receiver {
    /// The canonical head string — identity key and mangle base. Module
    /// qualified, because that is what a mangled name embeds.
    #[must_use]
    pub fn head_key(&self) -> MangledName {
        match self {
            Receiver::Type(n) => MangledName::new(n.to_mangled()),
            Receiver::Ref(k) => MangledName::new(k.prefix()),
            Receiver::Projection { base, assoc } => MangledName::new(format!("{base}::{assoc}")),
        }
    }

    /// The name an `impl` header writes its target as — no module, no type
    /// arguments.
    ///
    /// A different namespace from [`Self::head_key`], and keeping the two
    /// apart is the point: an impl-header scan compares against this one, and
    /// feeding it a mangled head matched nothing (silently losing a trait
    /// segment, a resource requirement, or a go-to-definition edge).
    #[must_use]
    pub fn decl_key(&self) -> DeclName {
        match self {
            Receiver::Type(n) => n.decl_name(),
            Receiver::Ref(k) => DeclName::new(k.prefix()),
            Receiver::Projection { base, assoc } => DeclName::new(format!("{base}::{assoc}")),
        }
    }

    /// Mangle the receiver with its type args (`Point<i32>`, `&List<i32>`,
    /// `S::SeqSerializer`).
    #[must_use]
    pub fn mangle(&self, type_args: &[String]) -> String {
        Self::mangle_with_ref(self.head_key().as_mangled_str(), self.ref_kind(), type_args)
    }

    /// Mangle a base name, applying a `&` / `&mut` prefix when `ref_kind` marks
    /// a single-pointee reference receiver. The sole place a receiver becomes a
    /// `&`-prefixed string; `ref_kind` is typed metadata, never parsed from
    /// `base`.
    #[must_use]
    pub fn mangle_with_ref(base: &str, ref_kind: Option<RefKind>, type_args: &[String]) -> String {
        match ref_kind {
            Some(RefKind::Shared) if type_args.len() == 1 => format!("&{}", type_args[0]),
            Some(RefKind::Mut) if type_args.len() == 1 => format!("&mut {}", type_args[0]),
            _ => mangle_generic_name(base, type_args),
        }
    }

    /// The receiver's reference kind, or `None` for a value receiver.
    #[must_use]
    pub fn ref_kind(&self) -> Option<RefKind> {
        match self {
            Receiver::Ref(k) => Some(*k),
            _ => None,
        }
    }

    /// Whether the receiver is an associated-type projection (`S::SeqSerializer`).
    #[must_use]
    pub fn is_assoc_projection(&self) -> bool {
        matches!(self, Receiver::Projection { .. })
    }
}

/// Decompose `Type^Trait::method` into its `(type, trait)` parts, or `None` when
/// the name has no `^` trait segment.
pub fn split_trait_method_receiver(name: &str) -> Option<(&str, &str)> {
    let (ty, rest) = name.split_once('^')?;
    let trait_name = rest.split_once("::").map_or(rest, |(t, _)| t);
    Some((ty, trait_name))
}

/// The reference prefix a mangled name carries, and the rest.
fn split_ref_prefix(name: &str) -> (&str, &str) {
    for prefix in [RefKind::Mut.prefix(), RefKind::Shared.prefix()] {
        if let Some(rest) = name.strip_prefix(prefix) {
            let (sep, rest) = rest
                .strip_prefix(' ')
                .map_or(("", rest), |trimmed| (" ", trimmed));
            return (&name[..prefix.len() + sep.len()], rest);
        }
    }
    ("", name)
}

/// The head's name without its declaring module. A module may itself contain
/// `/` (`./sub/geom.wado`), so the split is on the last one; a head never
/// carries type arguments, which is what makes that safe here.
fn head_simple_name(head: &str) -> &str {
    head.rsplit('/').next().unwrap_or(head)
}

/// Split a mangled argument list on the commas that separate arguments,
/// ignoring those nested inside an argument's own brackets.
fn split_type_args(inner: &str) -> Vec<&str> {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in inner.char_indices() {
        match c {
            '<' | '[' => depth += 1,
            '>' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(&inner[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    args.push(&inner[start..]);
    args
}

/// Render a mangled type name the way source writes it: the declaring module
/// dropped from the head and from every type argument.
///
/// A diagnostic names a type the way the user typed it. The module an fq name
/// carries is an internal identity, so printing one raw turns `Point` into
/// `./geom.wado/Point` and `Option<String>` into
/// `core:prelude/types.wado/Option<core:prelude/string.wado/String>`.
#[must_use]
pub fn display_type_name(mangled: &str) -> String {
    let (prefix, rest) = split_ref_prefix(mangled);
    let Some(open) = rest.find('<') else {
        return format!("{prefix}{}", head_simple_name(rest));
    };
    let Some(inner) = rest[open + 1..].strip_suffix('>') else {
        return format!("{prefix}{}", head_simple_name(rest));
    };
    let args: Vec<String> = split_type_args(inner)
        .into_iter()
        .map(display_type_name)
        .collect();
    format!(
        "{prefix}{}<{}>",
        head_simple_name(&rest[..open]),
        args.join(",")
    )
}

/// Decompose a local method mangle into `(receiver, trait, method)`.
///
/// - `Point::sum` → `("Point", None, "sum")`
/// - `Point^Display::fmt` → `("Point", Some("Display"), "fmt")`
/// - `Stdout::write_via_stream` → `("Stdout", None, "write_via_stream")`
///
/// Returns `None` when there is no `::` method separator (a bare
/// free-function name). The receiver / trait may still carry type-argument
/// mangling (`Box<i32>^Ord::cmp`); this splits only on the `^` and `::`
/// separators and does not strip type args.
pub fn split_local_method_name(name: &str) -> Option<(&str, Option<&str>, &str)> {
    let sep = name.find("::")?;
    let (prefix, method) = (&name[..sep], &name[sep + 2..]);
    let (receiver, trait_name) = match prefix.find('^') {
        Some(caret) => (&prefix[..caret], Some(&prefix[caret + 1..])),
        None => (prefix, None),
    };
    Some((receiver, trait_name, method))
}

/// Rebuild a monomorphized method's base-name key: replace everything up to
/// and including the first `::` with `base::`, keeping only the method suffix.
///
/// - `rebase_monomorph_method("Box<i32>::get", "Box")` → `"Box::get"`
/// - `rebase_monomorph_method("List<i32>^Ord::cmp", "List")` → `"List::cmp"`
///
/// When `mangled` has no `::` (a bare name), returns `base` unchanged. Used by
/// the DCE call-graph keying to keep a monomorphized method mergeable with its
/// generic template.
pub fn rebase_monomorph_method(mangled: &str, base: &str) -> String {
    match mangled.find("::") {
        Some(pos) => format!("{base}::{}", &mangled[pos + 2..]),
        None => base.to_string(),
    }
}

impl LocalMethodName {
    /// The typed receiver shape — the query consumers use to reason about the
    /// receiver instead of parsing its mangled identity.
    #[must_use]
    pub fn receiver(&self) -> &Receiver {
        &self.receiver
    }

    /// The base receiver identity string: `Point`, `&`, `&mut`, `S::SeqSerializer`.
    #[must_use]
    pub fn base_struct_name(&self) -> String {
        self.receiver.head_key().into_string()
    }

    /// [`Self::base_struct_name`] as the receiver form a mangled name embeds —
    /// read straight off the typed receiver, never re-derived from a string.
    #[must_use]
    pub fn fq_base_struct_name(&self) -> FqTypeName {
        match &self.receiver {
            Receiver::Type(fq) => fq.clone(),
            // A `&` / `&mut` head and a projection name no declaration, so no
            // module qualifies them.
            Receiver::Ref(kind) => FqTypeName::builtin(kind.prefix()),
            Receiver::Projection { base, assoc } => {
                FqTypeName::builtin(&format!("{base}::{assoc}"))
            }
        }
    }

    /// The receiver's declaration name — what an `impl` header writes. See
    /// [`Receiver::decl_key`] for why this is a different namespace from
    /// [`Self::base_struct_name`].
    #[must_use]
    pub fn receiver_decl_key(&self) -> DeclName {
        self.receiver.decl_key()
    }

    /// [`Self::struct_name`] as the receiver form a mangled name embeds —
    /// rebuilt from the typed receiver and its structured arguments.
    #[must_use]
    pub fn fq_struct_name(&self) -> FqTypeName {
        self.fq_base_struct_name()
            .with_args(self.struct_type_args.clone())
    }

    /// The receiver's declaration name, without its module — the key form the
    /// WIT-derived CM interface registry uses (`Fields::new`). That registry
    /// knows nothing of Wado modules, so a static call on a WASI resource has
    /// to reach it by the declared name alone.
    #[must_use]
    pub fn receiver_decl_name(&self) -> DeclName {
        self.fq_base_struct_name().decl_name()
    }

    /// The receiver's reference kind, or `None` for a value receiver.
    #[must_use]
    pub fn ref_receiver(&self) -> Option<RefKind> {
        self.receiver.ref_kind()
    }

    /// Whether the receiver is an associated-type projection (`S::SeqSerializer`).
    #[must_use]
    pub fn receiver_is_assoc_projection(&self) -> bool {
        self.receiver.is_assoc_projection()
    }

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
    pub fn new(struct_name: FqTypeName, trait_name: Option<String>, method_name: String) -> Self {
        Self::of(Receiver::Type(struct_name), trait_name, method_name)
    }

    /// Construct a method name for a `&T` / `&mut T` ref-impl receiver.
    #[must_use]
    pub fn new_ref(kind: RefKind, trait_name: Option<String>, method_name: String) -> Self {
        Self::of(Receiver::Ref(kind), trait_name, method_name)
    }

    /// Construct from an explicit typed receiver — the single construction path;
    /// `new` / `new_ref` are thin typed wrappers. No string is inspected.
    #[must_use]
    pub fn of(receiver: Receiver, trait_name: Option<String>, method_name: String) -> Self {
        let base_trait_name = trait_name
            .as_deref()
            .map(|n| split_base_name(n).to_string());
        let struct_name = receiver.head_key().into_string();
        Self {
            receiver,
            struct_name,
            struct_type_args: Vec::new(),
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
        struct_name: FqTypeName,
        trait_name: Option<String>,
        method_name: String,
        method_type_args: Vec<String>,
    ) -> Self {
        debug_assert!(
            struct_name.args().is_empty(),
            "LocalMethodName::with_method_type_args() expects a base receiver without type args, got: {struct_name}"
        );
        let base_trait_name = trait_name
            .as_deref()
            .map(|n| split_base_name(n).to_string());
        Self {
            struct_name: struct_name.to_mangled(),
            receiver: Receiver::Type(struct_name),
            struct_type_args: Vec::new(),
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
    pub fn with_type_args(
        &self,
        impl_type_args: &[FqTypeName],
        method_type_args: &[String],
    ) -> Self {
        let mangled_struct = if impl_type_args.is_empty() {
            self.struct_name.clone()
        } else {
            let rendered: Vec<String> = impl_type_args.iter().map(FqTypeName::to_mangled).collect();
            self.receiver.mangle(&rendered)
        };
        Self {
            struct_name: mangled_struct,
            struct_type_args: impl_type_args.to_vec(),
            receiver: self.receiver.clone(),
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
    pub fn with_struct_type_args(&self, type_args: &[FqTypeName]) -> Self {
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
            mangle_generic_name(&base, trait_type_args)
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
    /// `resolved` is the receiver the parameter resolved to; the instantiated
    /// spelling and the base head both come from it, so they cannot disagree —
    /// passing them as two separate strings is how they used to.
    #[must_use]
    pub fn with_substituted_struct_name(&self, resolved: &FqTypeName) -> Self {
        Self {
            struct_name: resolved.to_mangled(),
            receiver: Receiver::Type(resolved.head_only()),
            struct_type_args: resolved.args().to_vec(),
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

    /// A monomorphization-invariant identity built from the base struct / trait
    /// names and the bare method name, dropping every type argument. A generic
    /// method (`Result<T, E>::unwrap`) and each of its instantiations
    /// (`Result<Fields, HeaderError>::unwrap`) share one key, whereas
    /// [`Self::to_mangled_name`] embeds the type args and so differs per
    /// instantiation. Used where a property of the method — not the
    /// instantiation — is being keyed (e.g. whether it takes `self` by value).
    pub fn base_dispatch_key(&self) -> String {
        match &self.base_trait_name {
            Some(trait_name) => {
                format!(
                    "{}^{}::{}",
                    self.base_struct_name(),
                    trait_name,
                    self.method_name
                )
            }
            None => format!("{}::{}", self.base_struct_name(), self.method_name),
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
            && self
                .fq_struct_name()
                .decl_name()
                .as_decl_str()
                .starts_with(CLOSURE_STRUCT_PREFIX)
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
/// - `core/rt/SomeType`
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

/// Build a core/rt function name.
///
/// Format: `core/rt/{name}`
///
/// Example: `core/rt/log_stdout`
pub fn build_core_rt_name(interner: &mut ModuleSourceInterner, name: &str) -> FreeFunctionName {
    FreeFunctionName::from_strs(interner, &["core", "rt"], name)
}

/// Validate that a string can name a module.
///
/// A module path is a filesystem representation, not a URI, so URI-unsafe
/// characters such as spaces are valid; only content that cannot name a file
/// (a NUL byte) is rejected. Special prefixes (`core:` / `wasi:` /
/// `http(s)://`) are opaque identifiers and always valid. The analyzer calls
/// this before loading to surface a clean error message.
///
/// Returns `Ok(())` if the path is valid, or `Err(message)` if invalid.
pub fn validate_module_path(path: &str) -> Result<(), String> {
    if has_special_prefix(path) {
        return Ok(());
    }

    // A bare module path is a filesystem path, not a URI: URI-unsafe
    // characters such as spaces are valid here. Reject only content that
    // cannot name a file.
    if path.contains('\0') {
        return Err("module path contains a NUL byte".to_string());
    }
    Ok(())
}

/// `true` for an opaque module identifier that is not a filesystem path and
/// must never be normalized: a scheme-qualified name (`core:` / `wasi:`) or a
/// remote URI (`http://` / `https://`).
fn has_special_prefix(path: &str) -> bool {
    path.starts_with("core:")
        || path.starts_with("wasi:")
        || path.starts_with("https://")
        || path.starts_with("http://")
}

/// Normalize a module path.
///
/// A module path is a filesystem representation, not a URI: it is normalized
/// lexically via [`crate::path::normalize`] (resolve `.`/`..`, collapse
/// duplicate slashes, unify separators to `/`) and never percent-encoded, so
/// URI-unsafe characters such as spaces survive intact. Special prefixes
/// (`core:` / `wasi:` / `http://` / `https://`) are opaque identifiers and are
/// returned verbatim.
///
/// This function is infallible: every filesystem path normalizes.
///
/// Examples:
/// - `./geometry.wado` → `./geometry.wado`
/// - `./sub/../geometry.wado` → `./geometry.wado`
/// - `./sub/./nested/../file.wado` → `./sub/file.wado`
/// - `foo//bar.wado` → `foo/bar.wado`
/// - `/home/user/My Project/x.wado` → `/home/user/My Project/x.wado`
pub fn normalize_module_path(path: &str) -> String {
    if has_special_prefix(path) {
        return path.to_string();
    }
    crate::path::normalize(path)
}

/// [`normalize_module_path`] gated by [`validate_module_path`]. Normalization
/// itself cannot fail, but callers accepting untrusted input (e.g. a CLI
/// `--symbol` notation) use this to reject a string that cannot name a module
/// with a clean error instead of letting it flow downstream.
pub fn try_normalize_module_path(path: &str) -> Result<String, String> {
    validate_module_path(path)?;
    Ok(normalize_module_path(path))
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

/// Canonicalize a resolved local module identity to the unique minimal form
/// for its physical file, relative to the entry directory `entry_dir`.
///
/// Composing relative steps from the importer ([`resolve_module_path`]) is not
/// canonical: a path that climbs *above* `entry_dir` and re-enters spells the
/// same file non-minimally (`../src/gen/p.wado` vs `./gen/p.wado`), which
/// lexical normalization cannot fold. Re-anchoring under `entry_dir` gives one
/// identity per file on every import path, so the loader interns each once
/// (#1423). Empty `entry_dir` (no entry context) returns the input normalized
/// and otherwise unchanged, so such callers never regress.
#[must_use]
pub fn canonical_local_path(entry_dir: &str, resolved: &str) -> String {
    if entry_dir.is_empty() {
        return normalize_module_path(resolved);
    }
    // `relative_path` normalizes both arguments, so the anchored join needs no
    // separate normalize pass.
    crate::path::relative_path(entry_dir, &format!("{entry_dir}/{resolved}"))
}

/// The entry directory of an entry [`ModuleSource`], for use as the
/// [`canonical_local_path`] anchor. `EntryPoint`'s filename's parent; empty
/// for any other source or a parentless filename (both disable
/// canonicalization, falling back to plain relative composition).
#[must_use]
pub fn entry_dir_of(entry_module: Option<&ModuleSource>) -> String {
    match entry_module {
        Some(ModuleSource::EntryPoint { filename }) => module_parent_dir(filename).to_string(),
        _ => String::new(),
    }
}

/// The parent directory of a module path (everything before the last `/`, or
/// empty if none). Shared by every site that derives a [`canonical_local_path`]
/// anchor, so they agree on it.
#[must_use]
pub fn module_parent_dir(path: &str) -> &str {
    get_parent_path(path)
}

/// The symbol name of a module-level global var: `global:{module_source}::{name}`.
/// Every module is qualified the same way — there is no entry-module special
/// case, so the declaration and every read/write produce identical names by
/// construction and can never diverge.
#[must_use]
pub fn global_name(module_source: &ModuleSource, name: impl fmt::Display) -> String {
    format!("global:{module_source}::{name}")
}

/// The canonical loader identity for a relative `import_source` imported from a
/// local module `from_path`, anchored at `entry_dir`: compose
/// ([`resolve_module_path`]) then canonicalize ([`canonical_local_path`]). The
/// one resolver shared by the loader, the analyze/elaborator re-resolution, and
/// the CLI Kiln harvest, so all three agree on identities.
#[must_use]
pub fn resolve_local_identity(entry_dir: &str, from_path: &str, import_source: &str) -> String {
    canonical_local_path(entry_dir, &resolve_module_path(from_path, import_source))
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
            ModuleSource::Local { path } | ModuleSource::Dependency { path } => path.as_str(),
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

    // Relative import from within a dependency module stays inside that
    // dependency package (resolved against the importing dependency file).
    if let ModuleSource::Dependency { path } = from_module
        && (import_source.starts_with("./") || import_source.starts_with("../"))
    {
        let resolved = resolve_module_path(path, import_source);
        return interner.dependency(&resolved);
    }

    // Dependency name (`use { … } from "router"` / `from "ns:pkg"`): resolve
    // against `[dependencies]` before treating it as a relative sibling file.
    // Only the consuming project resolves its own `[dependencies]`; a bare
    // import from within a dependency must not bind to the consumer's deps. A
    // path dependency is Wado source; a registry dependency is a prebuilt
    // component imported across the CM boundary.
    if !import_source.starts_with("./")
        && !import_source.starts_with("../")
        && !matches!(from_module, ModuleSource::Dependency { .. })
    {
        if let Some(dep) = interner.resolve_dependency(import_source) {
            return dep;
        }
        if let Some(component) = interner.resolve_component_dependency(import_source) {
            return component;
        }
    }

    // Handle relative imports from local modules
    // For entry points, we don't resolve against the filename - just use the import directly
    if let ModuleSource::Local { path: from_path } = from_module
        && (from_path.starts_with("./") || from_path.starts_with("../"))
    {
        let resolved =
            resolve_local_identity(&entry_dir_of(entry_module), from_path, import_source);
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

    // Entry imports canonicalize against the entry dir, mirroring the loader.
    if matches!(from_module, ModuleSource::EntryPoint { .. }) {
        let resolved = canonical_local_path(
            &entry_dir_of(entry_module),
            &normalize_module_path(import_source),
        );
        return interner.local(&resolved);
    }

    // Fallback: normalize and return as Local path (bare/other imports).
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
    /// A built-in tuple `[T1, T2, …]` with element names already resolved
    Tuple(Vec<String>),
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
        TypeNameInfo::Tuple(elems) => mangle_tuple_type(&elems),
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

/// Mangle a tuple as `[T1,T2,…]`, e.g. `["i32","String"]` → `"[i32,String]"`.
pub fn mangle_tuple_type(elems: &[String]) -> String {
    format!("[{}]", elems.join(","))
}

/// The head name of a tuple type. Name formats live here, so
/// [`crate::tir::TypeTable::TUPLE_TYPE_NAME`] reads it from this one place.
pub const TUPLE_TYPE_NAME: &str = "[]";

/// A name in the *declaration* namespace: what source writes, what an `impl`
/// header spells, and what every by-name declaration lookup keys on — module
/// scope (`struct_fields`, `enum_case`, …), the CM interface registry,
/// go-to-definition.
///
/// Distinct from a mangled name, which carries the declaring module and which
/// no declaration lookup stores. Every naming defect in WEP 2026-07-28 was one
/// substituted for the other, and both being `String` is what let that compile.
/// So this deliberately has no `Deref<Target = str>`, no `AsRef<str>` and no
/// `From<String>`: it is minted by the authorities that know the namespace —
/// [`FqTypeName`], [`Receiver`], [`crate::tir::TypeTable`] — and read back only
/// through [`Self::as_decl_str`], which names what it is handing out.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeclName(String);

impl DeclName {
    /// Mint from a name already known to be in the declaration namespace —
    /// read off a declaration, an `impl` header, or an import scope. Restricted
    /// to this crate so the namespace has a bounded set of entry points.
    #[must_use]
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The underlying spelling. Named rather than a `Deref` so a consumer that
    /// wants a mangled name cannot reach one by accident.
    #[must_use]
    pub fn as_decl_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for DeclName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A name in the *mangled* namespace: module-qualified, type arguments
/// rendered — what an emitted function name embeds and what `func_map` keys on.
///
/// The counterpart to [`DeclName`], and deliberately not convertible to it. A
/// mangled head fed to a declaration lookup matched nothing and silently lost a
/// trait segment, a resource requirement, or a go-to-definition edge; the two
/// being interchangeable `String`s is what let that compile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MangledName(String);

impl MangledName {
    /// Mint from a spelling already in the mangled namespace — produced by
    /// [`FqTypeName::to_mangled`] or by a mangler that qualifies the same way.
    #[must_use]
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_mangled_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for MangledName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `Type::method` path in the declaration namespace — the key the CM
/// interface registry and the import-adapter map store.
///
/// Built from a [`DeclName`] receiver rather than assembled with `format!` at
/// each call site, so the half that decides the namespace cannot be a mangled
/// name. Two of the three sites that assembled this by hand were passing
/// `head_key()`, which the registry never stores.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeclPath(String);

impl DeclPath {
    /// `<receiver>::<method>` — a method reached through its declaring type.
    #[must_use]
    pub fn method_of(receiver: &DeclName, method: &str) -> Self {
        Self(format!("{receiver}::{method}"))
    }

    /// A path already spelled in the declaration namespace — a WIT-derived
    /// `interface::function` key, or one read back out of the registry.
    #[must_use]
    pub(crate) fn from_declared(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    #[must_use]
    pub fn as_decl_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for DeclPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Comparing against a literal is sound in either direction: a literal spells
/// one name, and for the builtins these comparisons ask about (`u128`, `i128`)
/// the declaration and mangled namespaces coincide. What the newtype prevents
/// is a `DeclName` being *passed* where a mangled name is expected.
impl PartialEq<str> for DeclName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for DeclName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for DeclName {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
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

/// Canonical struct-type-argument names for a closure / [`CLOSURE_FN_TRAIT`]
/// receiver: `[arity, return-type]`. Feeding these to
/// [`mangle_generic_name`] with `CLOSURE_FN_TRAIT` reproduces the
/// [`mangle_fn_type`] head, so the two spellings cannot drift. Trait synthesis
/// (`Fn<N,Ret>^Inspect`) and template expansion both build them here.
///
/// Examples:
/// - `fn_type_arg_names(2, "i32")` → `["2", "i32"]`
pub fn fn_type_arg_names(arity: usize, return_type_name: &str) -> Vec<String> {
    vec![arity.to_string(), return_type_name.to_string()]
}

/// [`fn_type_arg_names`] in the structured namespace.
#[must_use]
pub fn fn_type_args(arity: usize, return_type: &FqTypeName) -> Vec<FqTypeName> {
    vec![FqTypeName::arity(arity), return_type.clone()]
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

pub fn cm_wrap_async_func_name(interface_name: &str, method_name: &str) -> String {
    format!("__cm_wrap_async__{interface_name}_{method_name}")
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

/// Build the exported function name for a test block. The prefix encodes the
/// test's attributes, the index disambiguates anonymous tests, and `name` (when
/// present) is appended as an ASCII snake-case segment via [`test_name_to_snake`].
///
/// - `__test_{index}` / `__test_{index}_{snake}` — plain
/// - `__test_trap_…` — `#[expect_trap]`
/// - `__test_todo_…` — `#[TODO]` (or module-level `#[TODO]`)
/// - `__test_tm{ms}_…` — `#[timeout_ms(ms)]` (combines: `__test_trap_tm{ms}_…`)
///
/// The single source of this format: both the annotate walk and reify call here
/// so the two never drift.
pub fn test_function_name(
    meta: &crate::ast::TestMetadata,
    test_index: usize,
    name: Option<&str>,
) -> String {
    let prefix = match (meta.is_todo, meta.expect_trap, meta.timeout_ms) {
        (true, _, Some(ms)) => format!("__test_todo_tm{ms}"),
        (true, _, None) => "__test_todo".to_string(),
        (_, true, Some(ms)) => format!("__test_trap_tm{ms}"),
        (_, true, None) => "__test_trap".to_string(),
        (_, _, Some(ms)) => format!("__test_tm{ms}"),
        (_, _, None) => "__test".to_string(),
    };
    match name {
        Some(name) => format!("{prefix}_{test_index}_{}", test_name_to_snake(name)),
        None => format!("{prefix}_{test_index}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_type_name_in_mangled_matches_boundaries_only() {
        // Type-argument positions rewrite.
        assert_eq!(
            replace_type_name_in_mangled("List<Old>::with_capacity", "Old", "New"),
            "List<New>::with_capacity"
        );
        assert_eq!(
            replace_type_name_in_mangled("Map<Old, Old>.insert", "Old", "New"),
            "Map<New, New>.insert"
        );
        // Nested generic old names rewrite as a unit.
        assert_eq!(
            replace_type_name_in_mangled(
                "List<Tuple<String, List<u8>>>::push",
                "Tuple<String, List<u8>>",
                "Pair"
            ),
            "List<Pair>::push"
        );
        // A name merely containing the old name as a fragment is untouched.
        assert_eq!(
            replace_type_name_in_mangled("List<OldExtended>::push", "Old", "New"),
            "List<OldExtended>::push"
        );
        assert_eq!(
            replace_type_name_in_mangled("List<VeryOld>::push", "Old", "New"),
            "List<VeryOld>::push"
        );
        // Consecutive occurrences forming one identifier stay untouched: the
        // left context must survive across matches.
        assert_eq!(
            replace_type_name_in_mangled("List<OldOld>::push", "Old", "New"),
            "List<OldOld>::push"
        );
    }

    #[test]
    fn bare_dep_resolves_only_for_consumer_not_inside_dependency() {
        let mut interner = ModuleSourceInterner::new();
        let mut index = crate::compiler_host::DependencyIndex::default();
        index
            .resolved
            .insert("logger".to_string(), "../logger/src/lib.wado".to_string());
        interner.set_dependencies(index);
        let entry = interner.entry_point("main.wado");
        // From the consuming project, a bare name binds to its dependency.
        let from_entry = resolve_import_with_entry(&mut interner, &entry, "logger", None);
        assert!(matches!(from_entry, ModuleSource::Dependency { .. }));
        // From inside a dependency, the same bare name must NOT bind to the
        // consumer's deps — it falls through to the local fallback.
        let from_dep = interner.dependency("../greet/src/lib.wado");
        let resolved = resolve_import_with_entry(&mut interner, &from_dep, "logger", None);
        let logger_dep = interner.dependency("../logger/src/lib.wado");
        assert_ne!(resolved, logger_dep);
    }

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
    fn mangle_local_item_name_is_space_independent() {
        // The storage name only disambiguates local items *within* a module
        // (the module source qualifies it across modules), so it must depend
        // solely on the module-local `local` index — never on the
        // process-global `AstIdSpace`, which varies with unrelated parse
        // history and would otherwise leak into compiler output (mangled WIR
        // function names), making it non-deterministic.
        use crate::ast::{AstId, AstIdSpace};
        let space_a = AstIdSpace::next();
        let space_b = AstIdSpace::next();
        assert_ne!(space_a, space_b);
        let a = mangle_local_item_name("UserId", AstId::new(space_a, 19));
        let b = mangle_local_item_name("UserId", AstId::new(space_b, 19));
        assert_eq!(a, b, "mangled name must not encode the AstIdSpace");
        // Distinct declaration sites within a module still get distinct names.
        assert_ne!(a, mangle_local_item_name("UserId", AstId::new(space_a, 20)));
        // The declared name is still recoverable.
        assert_eq!(strip_local_item_id(&a), "UserId");
    }

    #[test]
    fn test_method_name_to_string_simple() {
        let mut interner = ModuleSourceInterner::new();
        let module = interner.local("./geometry.wado");
        let method = MethodName::new(
            module.clone(),
            FqTypeName::declared(&module, "Point"),
            None,
            "sum".to_string(),
        );
        assert_eq!(method.to_string(), "./geometry.wado/Point::sum");
    }

    #[test]
    fn test_method_name_to_string_with_trait() {
        let mut interner = ModuleSourceInterner::new();
        let module = interner.local("./geometry.wado");
        let method = MethodName::new(
            module.clone(),
            FqTypeName::declared(&module, "Point"),
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
        let func = FreeFunctionName::from_strs(&mut interner, &["core", "rt"], "log_stdout");
        assert_eq!(func.to_string(), "core/rt/log_stdout");
    }

    #[test]
    fn test_free_function_name_entry_is_qualified() {
        // The entry point is qualified by its portable base name like any other
        // module — no bare-name special case. The synthetic empty path renders
        // its `<entry>` placeholder; a real entry contributes its base name,
        // dropping the compile directory.
        let mut interner = ModuleSourceInterner::new();
        let synthetic = FreeFunctionName::from_strs(&mut interner, &[], "main");
        assert_eq!(synthetic.to_string(), "<entry>/main");

        let entry = FreeFunctionName::from_module_source(
            &interner.entry_point("/abs/pkg/src/main.wado"),
            "main",
        );
        assert_eq!(entry.to_string(), "main.wado/main");
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
        let struct_name = StructName::from_strs(&mut interner, &["core", "rt"], "SomeType");
        assert_eq!(struct_name.to_string(), "core:rt/SomeType");
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
    fn test_build_core_rt_name() {
        let mut interner = ModuleSourceInterner::new();
        let name = build_core_rt_name(&mut interner, "log_stdout");
        assert_eq!(name.to_string(), "core/rt/log_stdout");
        assert_eq!(name.module_source, ModuleSource::rt());
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
    fn test_resolve_absolute_base_with_space() {
        // The Kiln harvest uses the entry's absolute filesystem path as the
        // resolve base; a directory with a space must not panic and must keep
        // the absolute root (regression for #1417).
        assert_eq!(
            resolve_module_path("/abs/My Project/main.wado", "./eval.wado"),
            "/abs/My Project/eval.wado"
        );
        assert_eq!(
            resolve_module_path("/abs/My Project/sub/main.wado", "../eval.wado"),
            "/abs/My Project/eval.wado"
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
    fn test_normalize_module_path_filesystem_literal() {
        // A module path is a filesystem string: URI-unsafe characters such as
        // spaces are preserved, never percent-encoded, and never panic
        // (regression for #1417).
        assert_eq!(normalize_module_path("./a b.wado"), "./a b.wado");
        assert_eq!(
            normalize_module_path("/home/user/My Project/eval.wado"),
            "/home/user/My Project/eval.wado"
        );
        // A literal `%` is part of the filename, not an escape sequence.
        assert_eq!(normalize_module_path("./a%20b.wado"), "./a%20b.wado");
        // Absolute paths keep their root through dot-segment resolution.
        assert_eq!(normalize_module_path("/abs/a/../b.wado"), "/abs/b.wado");
    }

    #[test]
    fn test_validate_module_path_valid() {
        assert!(validate_module_path("./geometry.wado").is_ok());
        assert!(validate_module_path("../lib.wado").is_ok());
        assert!(validate_module_path("core:cli").is_ok());
        assert!(validate_module_path("wasi:filesystem").is_ok());
        assert!(validate_module_path("https://example.com/lib.wado").is_ok());
        assert!(validate_module_path("http://localhost:8080/lib.wado").is_ok());
        // A space is valid in a filesystem path (regression for #1417).
        assert!(validate_module_path("./a b.wado").is_ok());
        assert!(validate_module_path("/home/user/My Project/eval.wado").is_ok());
    }

    #[test]
    fn test_validate_module_path_invalid() {
        // Only content that cannot name a file is rejected (a NUL byte).
        assert!(validate_module_path("./file with\x00null.wado").is_err());
    }

    #[test]
    fn test_receiver_mangle() {
        let shared = Receiver::Ref(RefKind::Shared);
        assert_eq!(shared.mangle(&["i32".into()]), "&i32");
        assert_eq!(shared.mangle(&["List<i32>".into()]), "&List<i32>");
        let mutable = Receiver::Ref(RefKind::Mut);
        assert_eq!(mutable.mangle(&["String".into()]), "&mut String");
        assert_eq!(mutable.mangle(&["List<i32>".into()]), "&mut List<i32>");
        // Named receivers fall through to mangle_generic_name.
        assert_eq!(
            Receiver::Type(FqTypeName::builtin("List")).mangle(&["i32".into()]),
            "List<i32>"
        );
    }

    #[test]
    fn display_type_name_drops_modules_from_head_and_args() {
        assert_eq!(display_type_name("./geom.wado/Point"), "Point");
        // A naive split on the last `/` would answer `String>` here.
        assert_eq!(
            display_type_name("core:prelude/types.wado/Option<core:prelude/string.wado/String>"),
            "Option<String>"
        );
        assert_eq!(
            display_type_name("a.wado/Pair<b.wado/K,c.wado/Map<d.wado/V,i32>>"),
            "Pair<K,Map<V,i32>>"
        );
        // Builtin shapes carry no module, and refs keep their prefix.
        assert_eq!(display_type_name("i32"), "i32");
        assert_eq!(display_type_name("&./geom.wado/Point"), "&Point");
        assert_eq!(
            display_type_name("&mut a.wado/List<b.wado/T>"),
            "&mut List<T>"
        );
    }
}

/// Whether `name` names a builtin shape — one no module declares, so every
/// mangler spells it the same way wherever it appears. See
/// [`FqTypeName::builtin`].
///
/// An instantiated shape is its head: `Fn<1,i32>`, `Array<u8>` and `[]<A,B>`
/// are as module-less as the heads they instantiate.
#[must_use]
pub fn is_builtin_shape_name(name: &str) -> bool {
    name.starts_with('&')
        || matches!(
            split_base_name(name),
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "f32"
                | "f64"
                | "v128"
                | "bool"
                | "char"
                | "()"
                | "!"
                | "Array"
                | "[]"
                | "Fn"
        )
}

/// A receiver name in the form a mangled name may embed.
///
/// An fq name names its subject by the module that declares it, so a receiver
/// written into `Type::method` / `Type^Trait::method` must already carry that
/// module. The type exists to make the rule unforgeable: a bare `&str` read off
/// source text or off a `ResolvedType`'s `name` field cannot become a mangled
/// name by accident — it has to pass through one of the constructors below,
/// each of which states why its input is already fq.
/// The mangled spelling is a *rendering* ([`Self::to_mangled`]), produced on
/// demand and never parsed back. Every question a caller used to answer by
/// splitting the string — the declaring module, the declaration name, the type
/// arguments — is a field access here.
///
/// Splitting a rendered name apart is what this type exists to prevent. A
/// `ModuleSource` may itself contain `/` and `<`, and a type argument carries
/// its own module path, so no split on `/`, `<` or `,` is correct in general.
/// A rendered name is also not reversible: `ModuleSource` cannot be rebuilt
/// without the interner, so there is deliberately no constructor from a
/// mangled string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FqTypeName {
    /// Outermost `&` / `&mut`, when the receiver is a reference shape.
    reference: Option<RefKind>,
    head: TypeHead,
    /// Type arguments, already fq themselves.
    args: Vec<FqTypeName>,
}

/// What an [`FqTypeName`]'s head names. The three cases differ in whether a
/// module qualifies them — the distinction a bare `String` loses, and the one
/// every mis-dispatch in this area turned on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeHead {
    /// A declaration, named by the module that declares it.
    Declared {
        module: crate::module_source::ModuleSource,
        name: String,
    },
    /// A shape no module declares — a primitive, `()`, `!`, the raw GC `Array`,
    /// a function type. Every mangler spells one the same way.
    Builtin(String),
    /// A template's own type-parameter binder (`T`, a pack member `F`). Not a
    /// declaration, so it has no module.
    Binder(String),
    /// A tuple. Its own head, because a tuple is spelled `[a,b]` — not
    /// `Head<a,b>` like every other instantiated shape, which is what a
    /// `Builtin("[]")` carrying arguments would render as.
    Tuple,
}

impl TypeHead {
    /// The head's own name, as its declaration writes it.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Declared { name, .. } => name,
            Self::Builtin(name) | Self::Binder(name) => name,
            Self::Tuple => TUPLE_TYPE_NAME,
        }
    }

    /// The declaring module, or `None` for a head no module declares.
    #[must_use]
    pub fn module(&self) -> Option<&crate::module_source::ModuleSource> {
        match self {
            Self::Declared { module, .. } => Some(module),
            Self::Builtin(_) | Self::Binder(_) | Self::Tuple => None,
        }
    }
}

impl FqTypeName {
    fn of_head_kind(head: TypeHead) -> Self {
        Self {
            reference: None,
            head,
            args: Vec::new(),
        }
    }

    /// A declared type, named by the module that declares it. `module` must be
    /// the *declaring* module, not the use site's.
    ///
    /// `name` is the declaration's own name and carries no type arguments;
    /// [`Self::with_args`] adds them once the receiver is instantiated.
    #[must_use]
    pub fn declared(module: &crate::module_source::ModuleSource, name: &str) -> Self {
        Self::of_head_kind(TypeHead::Declared {
            module: module.clone(),
            name: name.to_string(),
        })
    }

    /// A template's own type-parameter binder (`T` in `impl<T: Bound> Trait for
    /// T`, a pack member `F`). A binder is not a declaration and has no module.
    #[must_use]
    pub fn binder(name: &str) -> Self {
        Self::of_head_kind(TypeHead::Binder(name.to_string()))
    }

    /// A tuple of `elems`, spelled `[a,b]`.
    #[must_use]
    pub fn tuple(elems: Vec<FqTypeName>) -> Self {
        Self::of_head_kind(TypeHead::Tuple).with_args(elems)
    }

    /// A builtin shape — a primitive, `()`, `!`, the raw GC `Array`, a
    /// reference, a function type. No module declares one, and every mangler
    /// spells it bare. A tuple has its own [`Self::tuple`]: it is not spelled
    /// `Head<args>`.
    #[must_use]
    pub fn builtin(name: &str) -> Self {
        Self::of_head_kind(TypeHead::Builtin(name.to_string()))
    }

    /// The arity that spells a [`CLOSURE_FN_TRAIT`] head's first argument
    /// (`Fn<2,i32>`). It names no type, so it mangles bare like a builtin shape.
    #[must_use]
    pub fn arity(arity: usize) -> Self {
        Self::of_head_kind(TypeHead::Builtin(arity.to_string()))
    }

    /// A head written in source, resolved against the module that declares it:
    /// [`Self::builtin`] for a builtin shape, [`Self::declared`] otherwise. The
    /// single place that decision is made, so a definition and a call site
    /// spelling the same head cannot disagree.
    #[must_use]
    pub fn of_head(module: &crate::module_source::ModuleSource, name: &str) -> Self {
        if is_builtin_shape_name(name) {
            Self::builtin(name)
        } else {
            Self::declared(module, name)
        }
    }

    /// The same head instantiated with `args`.
    #[must_use]
    pub fn with_args(mut self, args: Vec<FqTypeName>) -> Self {
        self.args = args;
        self
    }

    /// The same name behind a `&` / `&mut`.
    #[must_use]
    pub fn with_reference(mut self, kind: RefKind) -> Self {
        self.reference = Some(kind);
        self
    }

    #[must_use]
    pub fn head(&self) -> &TypeHead {
        &self.head
    }

    /// The same head with its type arguments dropped — the base receiver a
    /// dispatch template is named after.
    #[must_use]
    pub fn head_only(&self) -> Self {
        Self {
            reference: self.reference,
            head: self.head.clone(),
            args: Vec::new(),
        }
    }

    /// The declaration name of the head: no module, no type arguments. This is
    /// the form an `impl` header writes its target as, so it is what an
    /// impl-header scan compares against.
    #[must_use]
    pub fn decl_name(&self) -> DeclName {
        DeclName::new(self.head.name())
    }

    /// The module that declares this type, or `None` for a builtin or binder.
    #[must_use]
    pub fn module(&self) -> Option<&crate::module_source::ModuleSource> {
        self.head.module()
    }

    #[must_use]
    pub fn args(&self) -> &[FqTypeName] {
        &self.args
    }

    #[must_use]
    pub fn reference(&self) -> Option<RefKind> {
        self.reference
    }

    /// The mangled spelling embedded in a mangled method name.
    #[must_use]
    pub fn to_mangled(&self) -> String {
        let mut out = String::new();
        if let Some(kind) = self.reference {
            out.push_str(kind.prefix());
            out.push(' ');
        }
        if let TypeHead::Tuple = self.head {
            let elems: Vec<String> = self.args.iter().map(FqTypeName::to_mangled).collect();
            out.push_str(&mangle_tuple_type(&elems));
            return out;
        }
        match &self.head {
            TypeHead::Declared { module, name } => {
                out.push_str(&format!("{module}/{name}"));
            }
            TypeHead::Builtin(name) | TypeHead::Binder(name) => out.push_str(name),
            TypeHead::Tuple => unreachable!("handled above"),
        }
        if !self.args.is_empty() {
            let args: Vec<String> = self.args.iter().map(FqTypeName::to_mangled).collect();
            out.push('<');
            out.push_str(&args.join(","));
            out.push('>');
        }
        out
    }

    /// The declaration namespace: the name as source writes it, which is what
    /// an `impl` header spells and what every by-name declaration lookup keys
    /// on. Also the form diagnostics show.
    ///
    /// Modules dropped from the head and,
    /// recursively, from every type argument. Diagnostics only.
    #[must_use]
    pub fn to_display(&self) -> String {
        let mut out = String::new();
        if let Some(kind) = self.reference {
            out.push_str(kind.prefix());
            out.push(' ');
        }
        let args: Vec<String> = self.args.iter().map(FqTypeName::to_display).collect();
        if let TypeHead::Tuple = self.head {
            out.push_str(&mangle_tuple_type(&args));
            return out;
        }
        out.push_str(self.head.name());
        if !args.is_empty() {
            out.push('<');
            out.push_str(&args.join(","));
            out.push('>');
        }
        out
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.to_mangled()
    }
}

impl std::fmt::Display for FqTypeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_mangled())
    }
}
