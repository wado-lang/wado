//! Compiler-recognized stdlib items (the Wado compiler's analogue of
//! rustc's lang items).
//!
//! A `CompilerItem` identifies a specific stdlib symbol that the Wado
//! compiler — Rust-side — needs to reference directly: types it
//! instantiates (`Option<T>`, `Box<T>`), traits whose impls it
//! synthesises (`Default`, `From`, `Serialize`), or methods it lowers
//! into calls (`String::push_str`, `Array::push`). Each item is bound
//! to its resolution by a `#[compiler_item("...")]` attribute on the
//! Wado-side declaration; the resolver populates the
//! [`CompilerItems`] registry during the annotate phase and downstream
//! passes look symbols up through the registry instead of by string
//! name.
//!
//! Why this exists: pre-`CompilerItem`, the compiler hard-coded paths
//! and names (`LocalMethodName::new("String", None, "push_str")`,
//! `name == "Option"`) at every site. Renaming a stdlib item would
//! silently break those sites — failures showed up only at runtime as
//! "unresolved `MethodCall`" errors. With the registry, renames on the
//! Wado side are invisible to the Rust side as long as the
//! `#[compiler_item("...")]` value stays put; renaming the
//! `compiler_item` value, in turn, fails the compiler's own build by
//! breaking the enum variant ↔ string mapping in
//! [`CompilerItem::from_attr_name`].
//!
//! Scope: `#[compiler_item("...")]` is only meaningful inside
//! `core::*` modules. The resolver rejects the attribute on user code.

use std::fmt;

use crate::module_source::ModuleSource;

/// Every compiler-recognized stdlib item.
///
/// Each variant has a canonical `snake_case` name (see
/// [`CompilerItem::attr_name`]) that must match the argument of the
/// `#[compiler_item("...")]` attribute on the Wado-side declaration.
///
/// Naming convention: flat `snake_case`. Methods use
/// `<type>_<method>` (e.g. [`Self::StringPushStr`]); types and traits
/// use the lowercase type/trait name; variant cases use
/// `<variant>_<case>`. The convention is enforced socially, not by
/// the parser — the parser only checks that the argument names a
/// known [`CompilerItem`] variant.
#[derive(Copy, Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CompilerItem {
    // ── Types (structs / generic structs) ────────────────────────────
    /// `Box<T>` — boxes primitive values into a struct that
    /// participates in GC tracing.
    Box,
    /// `RangeExclusive<T>` — `a..b` literals.
    RangeExclusive,
    /// `RangeInclusive<T>` — `a..=b` literals.
    RangeInclusive,
    /// `Request<T>` — the Kiln CM adapter wraps decoded options in
    /// this struct.
    KilnRequest,

    // ── Variants (sum types) ──────────────────────────────────────────
    /// `Option<T>` — `Some(_)` / `None`.
    Option,
    /// `Result<T, E>` — `Ok(_)` / `Err(_)`.
    Result,

    // ── Traits ────────────────────────────────────────────────────────
    /// `Default` — `Default::default()` synthesis anchor.
    Default,
    /// `Eq` — anchor for synthesised `==` / `!=` lowering and the
    /// auto-derive checks that decide whether a compound type
    /// (struct, variant, generic instance) implements `Eq`.
    Eq,
    /// `From<T>` — synthesised by the `From` synthesiser.
    From,
    /// `core:serde::Serialize` — anchor for `Serialize` impl synthesis.
    Serialize,
    /// `core:serde::Deserialize` — anchor for `Deserialize` impl
    /// synthesis and for the Kiln CM adapter's options decoding.
    Deserialize,

    // ── Methods (impl-block functions) ────────────────────────────────
    /// `Array<T>::push` — recognised by the WIR optimiser to collapse
    /// `Array::new` + a sequence of `.push(...)` calls into
    /// `array.new_fixed`.
    ArrayPush,
    /// `String::push_str` — recognised by the WIR optimiser for
    /// string-building inlining.
    StringPushStr,
    /// `String::push_char` — recognised by the WIR optimiser for
    /// string-building inlining.
    StringPushChar,
    /// `String::get_byte_unchecked` — the unchecked byte read helper
    /// used by synthesised deserializers (`serde_synth`). Routed
    /// through this item so renames in the stdlib do not silently
    /// break code generation. See issue #1077.
    StringGetByteUnchecked,

    // ── Type families ─────────────────────────────────────────────────
    /// The tuple type family (`pub type [..T];`). The owning module is
    /// recorded so the compiler can synthesise tuple types under the
    /// correct module path.
    Tuple,
}

impl CompilerItem {
    /// Every variant, in declaration order. Used by validation passes
    /// that need to check the full registry.
    pub const ALL: &'static [CompilerItem] = &[
        Self::Box,
        Self::RangeExclusive,
        Self::RangeInclusive,
        Self::KilnRequest,
        Self::Option,
        Self::Result,
        Self::Default,
        Self::Eq,
        Self::From,
        Self::Serialize,
        Self::Deserialize,
        Self::ArrayPush,
        Self::StringPushStr,
        Self::StringPushChar,
        Self::StringGetByteUnchecked,
        Self::Tuple,
    ];

    /// Total number of variants. `ALL.len()` at compile time.
    pub const COUNT: usize = Self::ALL.len();

    /// Snake-case name used as the argument of
    /// `#[compiler_item("...")]`.
    ///
    /// This is the canonical wire form; renaming this string is a
    /// breaking change because stdlib annotations and Rust-side
    /// references must agree.
    pub fn attr_name(self) -> &'static str {
        match self {
            Self::Box => "box",
            Self::RangeExclusive => "range_exclusive",
            Self::RangeInclusive => "range_inclusive",
            Self::KilnRequest => "kiln_request",
            Self::Option => "option",
            Self::Result => "result",
            Self::Default => "default",
            Self::Eq => "eq",
            Self::From => "from",
            Self::Serialize => "serialize",
            Self::Deserialize => "deserialize",
            Self::ArrayPush => "array_push",
            Self::StringPushStr => "string_push_str",
            Self::StringPushChar => "string_push_char",
            Self::StringGetByteUnchecked => "string_get_byte_unchecked",
            Self::Tuple => "tuple",
        }
    }

    /// Resolve a `#[compiler_item("...")]` attribute argument back to
    /// the typed variant. Returns `None` for unknown names; the
    /// caller emits a diagnostic.
    pub fn from_attr_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|i| i.attr_name() == name)
    }

    /// The kind of declaration this item must be attached to. Used by
    /// the resolver to reject misuse like
    /// `#[compiler_item("option")]` on a trait.
    pub fn expected_kind(self) -> CompilerItemKind {
        match self {
            Self::Box | Self::RangeExclusive | Self::RangeInclusive | Self::KilnRequest => {
                CompilerItemKind::Struct
            }
            Self::Option | Self::Result => CompilerItemKind::Variant,
            Self::Default | Self::Eq | Self::From | Self::Serialize | Self::Deserialize => {
                CompilerItemKind::Trait
            }
            Self::ArrayPush
            | Self::StringPushStr
            | Self::StringPushChar
            | Self::StringGetByteUnchecked => CompilerItemKind::Method,
            Self::Tuple => CompilerItemKind::TupleFamily,
        }
    }
}

impl fmt::Display for CompilerItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.attr_name())
    }
}

/// The category of declaration a [`CompilerItem`] is attached to.
///
/// Kept separate from [`CompilerItem`] so the resolver can validate
/// the attribute placement (e.g. `Option` must be on a `variant`,
/// `Default` must be on a `trait`) and so the registry can offer
/// kind-specific accessors.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum CompilerItemKind {
    /// A `struct` declaration (`Box`, `Range*`, `KilnRequest`).
    Struct,
    /// A `variant` declaration (`Option`, `Result`).
    Variant,
    /// A `trait` declaration.
    Trait,
    /// A method inside an `impl` block.
    Method,
    /// The `pub type [..T];` declaration that owns the tuple family.
    TupleFamily,
}

impl fmt::Display for CompilerItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Struct => "struct",
            Self::Variant => "variant",
            Self::Trait => "trait",
            Self::Method => "method",
            Self::TupleFamily => "tuple type family",
        })
    }
}

/// The resolved data for a registered [`CompilerItem`].
///
/// One enum with a variant per [`CompilerItemKind`]. Each variant
/// carries enough information for downstream consumers to reconstruct
/// the lookup keys they need — most often the [`ModuleSource`] plus
/// the declared name (and, for methods, the owning type's name).
///
/// Consumers should generally not match on `Resolved` directly; use
/// the kind-checked accessors on [`CompilerItems`]
/// (e.g. [`CompilerItems::require_trait_module`]) instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolved {
    Struct {
        module_source: ModuleSource,
        name: String,
    },
    Variant {
        module_source: ModuleSource,
        name: String,
    },
    Trait {
        module_source: ModuleSource,
        name: String,
    },
    Method {
        module_source: ModuleSource,
        /// The type the method is defined on (e.g. `"String"`).
        owner_type: String,
        name: String,
    },
    /// The module that owns the tuple type family. Tuples have no
    /// user-visible declared name on the Wado side, only an owning
    /// module; the [`ModuleSource`] is therefore the only payload.
    TupleFamily { module_source: ModuleSource },
}

impl Resolved {
    /// The kind of this resolution. Always matches
    /// [`CompilerItem::expected_kind`] for the registered item.
    pub fn kind(&self) -> CompilerItemKind {
        match self {
            Self::Struct { .. } => CompilerItemKind::Struct,
            Self::Variant { .. } => CompilerItemKind::Variant,
            Self::Trait { .. } => CompilerItemKind::Trait,
            Self::Method { .. } => CompilerItemKind::Method,
            Self::TupleFamily { .. } => CompilerItemKind::TupleFamily,
        }
    }

    /// The defining module of this item. Always set, including for
    /// the tuple family.
    pub fn module_source(&self) -> &ModuleSource {
        match self {
            Self::Struct { module_source, .. }
            | Self::Variant { module_source, .. }
            | Self::Trait { module_source, .. }
            | Self::Method { module_source, .. }
            | Self::TupleFamily { module_source } => module_source,
        }
    }
}

/// Registry that binds each [`CompilerItem`] to a [`Resolved`].
///
/// Populated by the resolver during the annotate phase as it walks
/// stdlib modules and discovers `#[compiler_item("...")]` attributes.
/// Downstream passes (lower, synthesis, optimize, codegen) read from
/// the registry instead of hard-coding stdlib paths.
///
/// Storage is a dense array indexed by [`CompilerItem`] (cast via
/// [`CompilerItem::ALL`]'s declaration order); every lookup is O(1).
#[derive(Clone, Debug)]
pub struct CompilerItems {
    items: Vec<Option<Resolved>>,
}

impl Default for CompilerItems {
    /// Must delegate to [`Self::new`]. Deriving `Default` would
    /// produce an empty `items` vector and every accessor would
    /// panic with an out-of-bounds index — `index_of` returns
    /// `0..CompilerItem::COUNT`.
    fn default() -> Self {
        Self::new()
    }
}

impl CompilerItems {
    pub fn new() -> Self {
        Self {
            items: vec![None; CompilerItem::COUNT],
        }
    }

    /// Internal: convert a [`CompilerItem`] to its dense index.
    /// `CompilerItem::ALL` is the source of truth for the ordering.
    fn index_of(item: CompilerItem) -> usize {
        CompilerItem::ALL
            .iter()
            .position(|i| *i == item)
            .expect("CompilerItem must appear in CompilerItem::ALL")
    }

    /// Get the resolved data for `item`, or `None` if the stdlib has
    /// not yet registered it. Most call sites should prefer
    /// [`Self::require`] or the kind-specific `require_*` helpers.
    pub fn get(&self, item: CompilerItem) -> Option<&Resolved> {
        self.items[Self::index_of(item)].as_ref()
    }

    /// Return the resolved data, panicking with a clear ICE message
    /// if the item was not registered. Use this when a downstream
    /// pass cannot proceed without the item — the registry is
    /// validated at the end of stdlib loading, so a missing item
    /// here indicates a bug in either the stdlib or the validator.
    pub fn require(&self, item: CompilerItem) -> &Resolved {
        self.get(item).unwrap_or_else(|| {
            panic!(
                "compiler item `{item}` is not registered; \
                 expected a `#[compiler_item(\"{name}\")]` annotation \
                 on a {kind} in `core::*`",
                name = item.attr_name(),
                kind = item.expected_kind(),
            )
        })
    }

    /// Register `item` with `resolved`. Returns an error if the kind
    /// of `resolved` does not match [`CompilerItem::expected_kind`]
    /// or if the slot is already filled with a different resolution.
    ///
    /// Re-registering the same `Resolved` value is a no-op — this
    /// matches existing resolver behaviour where the same module may
    /// be visited multiple times during the annotate pass.
    pub fn register(
        &mut self,
        item: CompilerItem,
        resolved: Resolved,
    ) -> Result<(), RegisterError> {
        if resolved.kind() != item.expected_kind() {
            return Err(RegisterError::KindMismatch {
                item,
                expected: item.expected_kind(),
                actual: resolved.kind(),
            });
        }
        let idx = Self::index_of(item);
        match &self.items[idx] {
            Some(existing) if existing == &resolved => Ok(()),
            Some(existing) => Err(RegisterError::Duplicate {
                item,
                existing_module: existing.module_source().clone(),
                new_module: resolved.module_source().clone(),
            }),
            None => {
                self.items[idx] = Some(resolved);
                Ok(())
            }
        }
    }

    // ─── Kind-checked accessors ──────────────────────────────────────
    //
    // Each one panics on kind mismatch — that would mean
    // `CompilerItem::expected_kind` and the variant of `Resolved`
    // disagreed, which is impossible if `register` succeeded.

    /// Module + struct name of a [`CompilerItemKind::Struct`] item.
    pub fn require_struct(&self, item: CompilerItem) -> (&ModuleSource, &str) {
        match self.require(item) {
            Resolved::Struct {
                module_source,
                name,
            } => (module_source, name.as_str()),
            other => kind_mismatch_ice(item, "Struct", other),
        }
    }

    /// Module + variant name of a [`CompilerItemKind::Variant`] item
    /// (`Option`, `Result`).
    pub fn require_variant(&self, item: CompilerItem) -> (&ModuleSource, &str) {
        match self.require(item) {
            Resolved::Variant {
                module_source,
                name,
            } => (module_source, name.as_str()),
            other => kind_mismatch_ice(item, "Variant", other),
        }
    }

    /// Module-only accessor for a variant. Kept for migration of
    /// call sites that only need the module path (e.g.
    /// `TypeTable::make_option`).
    pub fn variant_module(&self, item: CompilerItem) -> Option<&ModuleSource> {
        match self.get(item)? {
            Resolved::Variant { module_source, .. } => Some(module_source),
            _ => None,
        }
    }

    /// Module + trait name of a [`CompilerItemKind::Trait`] item.
    pub fn require_trait(&self, item: CompilerItem) -> (&ModuleSource, &str) {
        match self.require(item) {
            Resolved::Trait {
                module_source,
                name,
            } => (module_source, name.as_str()),
            other => kind_mismatch_ice(item, "Trait", other),
        }
    }

    /// Name-only convenience for a [`CompilerItemKind::Trait`] item.
    /// Equivalent to `require_trait(item).1`; the dedicated helper
    /// keeps use sites readable when only the trait name is needed
    /// (e.g. when constructing a synthesised `LocalMethodName`).
    pub fn trait_name(&self, item: CompilerItem) -> &str {
        self.require_trait(item).1
    }

    /// Name-only convenience for a [`CompilerItemKind::Struct`] item.
    pub fn struct_name(&self, item: CompilerItem) -> &str {
        self.require_struct(item).1
    }

    /// Name-only convenience for a [`CompilerItemKind::Variant`] item.
    pub fn variant_name(&self, item: CompilerItem) -> &str {
        self.require_variant(item).1
    }

    /// Name-only convenience for a [`CompilerItemKind::Method`] item.
    /// Returns the unmangled method name (e.g. `"push_str"`).
    pub fn method_name(&self, item: CompilerItem) -> &str {
        self.require_method(item).2
    }

    /// Module-only accessor for a trait.
    pub fn trait_module(&self, item: CompilerItem) -> Option<&ModuleSource> {
        match self.get(item)? {
            Resolved::Trait { module_source, .. } => Some(module_source),
            _ => None,
        }
    }

    /// Module-only accessor for a struct.
    pub fn struct_module(&self, item: CompilerItem) -> Option<&ModuleSource> {
        match self.get(item)? {
            Resolved::Struct { module_source, .. } => Some(module_source),
            _ => None,
        }
    }

    /// Module + owner-type name + method name of a
    /// [`CompilerItemKind::Method`] item.
    pub fn require_method(&self, item: CompilerItem) -> (&ModuleSource, &str, &str) {
        match self.require(item) {
            Resolved::Method {
                module_source,
                owner_type,
                name,
            } => (module_source, owner_type.as_str(), name.as_str()),
            other => kind_mismatch_ice(item, "Method", other),
        }
    }

    /// Module that owns the tuple type family.
    pub fn tuple_module(&self) -> Option<&ModuleSource> {
        match self.get(CompilerItem::Tuple)? {
            Resolved::TupleFamily { module_source } => Some(module_source),
            _ => None,
        }
    }
}

#[cold]
#[inline(never)]
fn kind_mismatch_ice(item: CompilerItem, expected_variant: &str, got: &Resolved) -> ! {
    panic!(
        "compiler item `{item}` registered as {got_kind:?}, \
         but require_{expected_variant} was called",
        got_kind = got.kind(),
    )
}

/// Errors returned from [`CompilerItems::register`]. The resolver
/// converts these into user-facing diagnostics (or compiler ICEs
/// when the violation is in stdlib code).
#[derive(Clone, Debug)]
pub enum RegisterError {
    /// The annotated declaration is the wrong kind for this item —
    /// e.g. `#[compiler_item("option")]` was placed on a struct
    /// instead of a variant.
    KindMismatch {
        item: CompilerItem,
        expected: CompilerItemKind,
        actual: CompilerItemKind,
    },
    /// Two different declarations both claim the same item. The
    /// stdlib must contain exactly one `#[compiler_item("x")]`
    /// annotation for each `x`.
    Duplicate {
        item: CompilerItem,
        existing_module: ModuleSource,
        new_module: ModuleSource,
    },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KindMismatch {
                item,
                expected,
                actual,
            } => write!(
                f,
                "`#[compiler_item(\"{item}\")]` expects a {expected}, \
                 but it is attached to a {actual}"
            ),
            Self::Duplicate {
                item,
                existing_module,
                new_module,
            } => write!(
                f,
                "duplicate `#[compiler_item(\"{item}\")]`: already registered \
                 from `{existing_module}`, redeclared in `{new_module}`"
            ),
        }
    }
}

/// Parse all `#[compiler_item("...")]` arguments from a declaration's
/// attribute list.
///
/// Returns the matched [`CompilerItem`] values in encounter order,
/// plus a [`Vec`] of *unrecognised* argument strings so the resolver
/// can emit a diagnostic without losing the original spelling.
///
/// In valid stdlib code, a single declaration carries at most one
/// `#[compiler_item("...")]` attribute. Multiple matches are not
/// rejected here; the resolver decides whether to fold the bits
/// (legacy behaviour from the original `comp_feature` mechanism) or flag duplicates.
pub fn parse_compiler_item_attrs(
    attrs: &[crate::ast::Attribute],
) -> (Vec<CompilerItem>, Vec<String>) {
    let mut items = Vec::new();
    let mut unknown = Vec::new();
    for attr in attrs {
        if attr.name != "compiler_item" {
            continue;
        }
        for arg in &attr.args {
            let raw = arg.as_str();
            match CompilerItem::from_attr_name(raw) {
                Some(item) => items.push(item),
                None => unknown.push(raw.to_string()),
            }
        }
    }
    (items, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_variants_have_unique_attr_names() {
        let mut names: Vec<&str> = CompilerItem::ALL.iter().map(|i| i.attr_name()).collect();
        names.sort_unstable();
        let len_before = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            len_before,
            "duplicate attr_name across CompilerItem variants"
        );
    }

    #[test]
    fn attr_name_roundtrips() {
        for &item in CompilerItem::ALL {
            assert_eq!(CompilerItem::from_attr_name(item.attr_name()), Some(item));
        }
    }

    #[test]
    fn count_matches_all_len() {
        assert_eq!(CompilerItem::COUNT, CompilerItem::ALL.len());
    }

    #[test]
    fn register_rejects_kind_mismatch() {
        let mut reg = CompilerItems::new();
        let err = reg
            .register(
                CompilerItem::Option,
                Resolved::Trait {
                    module_source: ModuleSource::types(),
                    name: "Option".into(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, RegisterError::KindMismatch { .. }));
    }

    #[test]
    fn register_rejects_duplicate() {
        let mut reg = CompilerItems::new();
        let first = Resolved::Variant {
            module_source: ModuleSource::types(),
            name: "Option".into(),
        };
        let second = Resolved::Variant {
            module_source: ModuleSource::prelude(),
            name: "Option".into(),
        };
        reg.register(CompilerItem::Option, first).unwrap();
        let err = reg.register(CompilerItem::Option, second).unwrap_err();
        assert!(matches!(err, RegisterError::Duplicate { .. }));
    }

    #[test]
    fn register_same_value_is_noop() {
        let mut reg = CompilerItems::new();
        let resolved = Resolved::Variant {
            module_source: ModuleSource::types(),
            name: "Option".into(),
        };
        reg.register(CompilerItem::Option, resolved.clone())
            .unwrap();
        reg.register(CompilerItem::Option, resolved).unwrap();
    }

    #[test]
    fn require_kind_checked_accessors_work() {
        let mut reg = CompilerItems::new();
        reg.register(
            CompilerItem::Default,
            Resolved::Trait {
                module_source: ModuleSource::traits(),
                name: "Default".into(),
            },
        )
        .unwrap();
        let (module, name) = reg.require_trait(CompilerItem::Default);
        assert_eq!(name, "Default");
        assert_eq!(module, &ModuleSource::traits());
    }

    /// Regression: a `Default`-constructed registry must be usable
    /// like one from `new()`. A naive `#[derive(Default)]` would
    /// leave `items` empty and every accessor would index out of
    /// bounds.
    #[test]
    fn default_constructor_matches_new() {
        let reg = CompilerItems::default();
        for &item in CompilerItem::ALL {
            assert!(reg.get(item).is_none(), "{item} should start empty");
        }
    }
}
