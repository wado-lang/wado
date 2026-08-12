//! Global trait knowledge base: trait declarations, impl blocks, and blanket impls.
//!
//! `TraitEnv` is built once before resolution begins and is immutable thereafter.
//! It provides O(1) lookup of trait implementations by type name and trait name,
//! replacing linear scans across all modules.

use std::sync::Arc;

use crate::ast::{self, AstId, Item, Module, Type};
use crate::hashmap::{IndexMap, IndexSet};
use crate::kiln::InvocationIndex;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name;
use crate::tir::TypeTable;
use crate::token::Span;

/// A module's type-name import scope, derived once from its `use`
/// declarations. The single source of truth for how a module resolves a type
/// name, shared by its body walk and by sites that reconstruct its perspective.
#[derive(Clone, Default, Debug)]
pub(crate) struct ModuleImportScope {
    /// In-scope type name → declaring module, including `ns$Type` aliases for
    /// each namespace import's public type members.
    pub(super) sources: IndexMap<String, ModuleSource>,
    /// Aliased local name → original declaration name (`use { A as B }` and the
    /// `ns$Type` → `Type` namespace-member aliases).
    pub(super) original_names: IndexMap<String, String>,
    /// Namespace-import alias (`use ns from "…"`) → the namespace's module.
    /// Drives `ns::Type` resolution (issue #1415).
    pub(super) namespace_imports: IndexMap<String, ModuleSource>,
}

/// Compute a module's import scope from its `use` declarations. Pure over
/// `(module, from_module, entry_module, invocations, symbols)` plus the
/// (idempotent, loader-warmed) interner; safe to memoize per module. This is
/// the body behind `Elaborator::build_imported_type_sources`, lifted to a free
/// function so [`TraitEnv::build`] can pre-compute the per-module scopes
/// without an `Elaborator`/host in hand. `symbols` is needed to enumerate a
/// namespace import's public type members.
pub(super) fn module_import_scope(
    interner: &mut ModuleSourceInterner,
    module: &Module,
    from_module: &ModuleSource,
    entry_module: Option<&ModuleSource>,
    invocations: &InvocationIndex,
    symbols: &SymbolTable,
) -> ModuleImportScope {
    let mut scope = ModuleImportScope::default();
    // Case (variant/enum/flags) names brought in by imported types. Applied
    // after every type name is in scope, so a type always shadows a same-named
    // case (e.g. a `FieldKind::List` case must not hide the prelude `List`
    // type). Collected here, inserted with `or_insert` at the end.
    let mut pending_cases: Vec<(String, ModuleSource)> = Vec::new();
    for item in &module.items {
        if let Item::Use(use_decl) = item {
            let source = crate::loader::resolve_use_decl_source(
                interner,
                from_module,
                use_decl,
                entry_module,
                invocations,
            );
            for use_item in &use_decl.items {
                match use_item {
                    ast::UseItem::Simple { name, alias, .. } => {
                        let local_name = alias.as_ref().unwrap_or(name);
                        // Resolve through re-export chains so a name imported
                        // from a `pub use` barrel records its true definer
                        // module — not the barrel, which doesn't register the
                        // type, so `lookup_ref` would otherwise miss it and
                        // resolve to nothing (issue #1416).
                        let resolved = symbols.lookup_in_module(&source, name);
                        let (def_source, def_name) = resolved
                            .map(|sym| (sym.module_source().clone(), sym.name.clone()))
                            .unwrap_or_else(|| (source.clone(), name.clone()));
                        scope.sources.insert(local_name.clone(), def_source.clone());
                        if local_name != &def_name {
                            scope.original_names.insert(local_name.clone(), def_name);
                        }
                        // Importing a variant/enum/flags type brings its case
                        // names into scope so bare `Some` / `Ok` / enum cases
                        // resolve through the import branch.
                        if let Some(sym) = resolved {
                            collect_case_names(&mut pending_cases, sym, &def_source);
                        }
                    }
                    ast::UseItem::Namespace { name: ns } => {
                        // Expand each public type member to its `ns$Type` alias.
                        for sym in symbols.get_module_symbols(&source) {
                            if matches!(
                                sym.kind,
                                crate::symbol::SymbolKind::Struct(_)
                                    | crate::symbol::SymbolKind::Enum(_)
                                    | crate::symbol::SymbolKind::Flags(_)
                                    | crate::symbol::SymbolKind::Variant(_)
                                    | crate::symbol::SymbolKind::Newtype(_)
                                    | crate::symbol::SymbolKind::Resource(_)
                                    | crate::symbol::SymbolKind::BuiltinType
                            ) {
                                let alias = name::namespace_member_alias(ns, &sym.name);
                                scope.sources.insert(alias.clone(), source.clone());
                                scope.original_names.insert(alias, sym.name.clone());
                            }
                        }
                        scope.namespace_imports.insert(ns.clone(), source.clone());
                    }
                    ast::UseItem::InterfaceFunctions { .. } | ast::UseItem::Wildcard => {}
                }
            }
        }
    }

    // Auto-import the prelude: inject `core:prelude`'s exported type names into
    // the scope so prelude types (`String`, `List`, `Option`, …) resolve
    // through the import branch like any `use`. Modules opting out
    // (`#![no_prelude]` — the prelude sub-modules and
    // `core:rt`/`builtin`/`allocator`) import explicitly. Explicit `use`
    // items above take precedence, so they are not overwritten.
    if !module.has_no_prelude() {
        let prelude = ModuleSource::prelude();
        for name in symbols.reexport_names(&prelude) {
            if scope.sources.contains_key(&name) {
                continue;
            }
            let Some(sym) = symbols.lookup_in_module(&prelude, &name) else {
                continue;
            };
            if !matches!(
                sym.kind,
                crate::symbol::SymbolKind::Struct(_)
                    | crate::symbol::SymbolKind::Enum(_)
                    | crate::symbol::SymbolKind::Flags(_)
                    | crate::symbol::SymbolKind::Variant(_)
                    | crate::symbol::SymbolKind::Newtype(_)
                    | crate::symbol::SymbolKind::Resource(_)
                    | crate::symbol::SymbolKind::BuiltinType
                    | crate::symbol::SymbolKind::Trait(_)
            ) {
                continue;
            }
            let def_source = sym.module_source().clone();
            scope.sources.insert(name.clone(), def_source.clone());
            if name != *sym.name {
                scope.original_names.insert(name, sym.name.clone());
            }
            collect_case_names(&mut pending_cases, sym, &def_source);
        }
    }

    // Apply case names last, never overwriting a type name: a type always wins
    // a name clash with a case (see `pending_cases`).
    for (case, src) in pending_cases {
        scope.sources.entry(case).or_insert(src);
    }

    scope
}

/// Collect a variant/enum/flags symbol's case (or member) names so they can
/// resolve unqualified (`Some`, `Ok`, an enum case used bare), pointing at the
/// type's defining module. Other symbol kinds are ignored. Cases cannot be
/// aliased, so each name maps to itself.
fn collect_case_names(
    pending: &mut Vec<(String, ModuleSource)>,
    sym: &crate::symbol::Symbol,
    def_source: &ModuleSource,
) {
    use crate::symbol::SymbolKind;
    let cases: &[String] = match &sym.kind {
        SymbolKind::Variant(v) => &v.cases,
        SymbolKind::Enum(e) => &e.cases,
        SymbolKind::Flags(f) => &f.members,
        _ => return,
    };
    for case in cases {
        pending.push((case.clone(), def_source.clone()));
    }
}

use super::types::TypeError;
use crate::symbol::SymbolTable;

/// Pick a `ModuleSource` out of the AST + synthesised candidate lists. A
/// `prefer` hint wins wherever it appears; otherwise the first AST entry
/// answers, then the first synthesised one.
///
/// AST-first is load-bearing: where a type carries both a written `impl` block
/// and generated code, the block's module is the answer, and reordering these
/// routes serde types to the wrong module.
///
/// The union is what keeps one layer from masking the other on a shared key —
/// core's variadic `impl<..T> Inspect for [..T]` and a user `struct Tuple`'s
/// auto-derived `Tuple^Inspect` collide there, and the hint separates them.
fn pick_module_union<'a>(
    ast: Option<&'a Vec<ModuleSource>>,
    syn: Option<&'a Vec<ModuleSource>>,
    prefer: Option<&ModuleSource>,
) -> Option<&'a ModuleSource> {
    let in_list = |list: Option<&'a Vec<ModuleSource>>, hint: &ModuleSource| {
        list.and_then(|l| l.iter().find(|m| *m == hint))
    };
    if let Some(hint) = prefer
        && let Some(m) = in_list(ast, hint).or_else(|| in_list(syn, hint))
    {
        return Some(m);
    }
    ast.and_then(|l| l.first())
        .or_else(|| syn.and_then(|l| l.first()))
}

/// Canonical key for a declaration that lives in some module. Used by every
/// trait / effect / resource / type index in this file so that two modules
/// can host declarations with the same bare name (`pub interface Logger`
/// in both `mod_a` and `mod_b`) without their entries colliding on a
/// single `String` slot. The defining module is intrinsic to identity for
/// these kinds — the elaborator canonicalises a use-site bare name through
/// the symbol table ([`crate::elaborator::Elaborator::canonical_decl_key`])
/// before consulting the index.
pub(crate) type DeclKey = (ModuleSource, String);

/// Pre-built index: type name → list of (`ModuleSource`, item index) for trait impl blocks.
/// Built once from all loaded modules to avoid O(all items) scans per method call.
///
/// Keyed by the bare receiver type name on purpose: the lookup iterates the
/// candidate `Vec` and disambiguates each entry via its `(ModuleSource,
/// AstId)` payload plus the elaborator's per-call type-id comparison, so
/// two `struct Widget` declarations in different modules share one bucket
/// without ambiguity.
/// Identity of an impl's target type. A named type keys by its *declaring*
/// module and canonical name, so two modules' same-named structs — and one
/// type reached under an alias — are the same key exactly when they are the
/// same type. A `&T` / `&mut T` target is universal and declares nothing, so
/// it keys by reference kind alone.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ImplTargetKey {
    Decl(DeclKey),
    Ref(name::RefKind),
    /// A blanket impl's bare type parameter (`impl<T> Display for T`). It
    /// names no declaration, so it gets no `DeclKey`: a lookup starts from a
    /// type and can never reach this variant, which is what keeps a blanket
    /// impl out of the bucket of a type that happens to share the parameter's
    /// name. The module is the impl's own — the parameter is scoped to it.
    TypeParam(ModuleSource, String),
    /// A shape no module declares — the tuple family (`[..T]`), a function
    /// type. It reaches no declaration by name, so no module qualifies it, and
    /// a definition and a lookup agree without either knowing where the other
    /// stood. A primitive is *not* here: `i32` and `()` are `internal type`
    /// declarations and resolve like any other name.
    Builtin(String),
}

impl ImplTargetKey {
    /// The key for a head written in `module`.
    ///
    /// The declaration the head names, from the resolution table. A shape that
    /// names none drops the module; a name that reaches nothing at all keeps
    /// it, since the writing module is the only vantage left.
    pub(crate) fn of_written(
        name: &str,
        module: &ModuleSource,
        symbols: &SymbolTable,
        resolutions: &crate::resolve::Resolutions,
    ) -> Self {
        let (declaring, declared) = resolutions
            .declaration_named(module, name, symbols)
            .unwrap_or_else(|| (module.clone(), name.to_string()));
        Self::of_decl(&declaring, &declared)
    }

    /// The key for a declaration already identified.
    ///
    /// The one place the builtin decision is made, mirroring
    /// [`name::FqTypeName::of_head`]: a shape no module declares drops its
    /// module, so a definition reached through a written head and a lookup
    /// reached through a resolved type land on the same key.
    pub(crate) fn of_decl(module: &ModuleSource, name: &str) -> Self {
        if name::is_builtin_shape_name(name) {
            return ImplTargetKey::Builtin(name.to_string());
        }
        ImplTargetKey::Decl((module.clone(), name.to_string()))
    }
    /// The receiver this target indexes under. Built from the same
    /// `(module, name)` pair `TypeTable::impl_receiver_key` reads off a
    /// resolved type, so a definition and a lookup agree by construction.
    pub(crate) fn receiver(&self) -> name::Receiver {
        match self {
            ImplTargetKey::Decl((module, name)) => {
                name::Receiver::Type(name::FqTypeName::of_head(module, name))
            }
            // A type parameter names no declaration, so no module qualifies it.
            ImplTargetKey::TypeParam(_, name) => {
                name::Receiver::Type(name::FqTypeName::binder(name))
            }
            ImplTargetKey::Ref(kind) => name::Receiver::Ref(*kind),
            ImplTargetKey::Builtin(name) => name::Receiver::Type(name::FqTypeName::builtin(name)),
        }
    }

    pub(crate) fn type_name(&self) -> Option<&str> {
        match self {
            ImplTargetKey::Decl((_, name))
            | ImplTargetKey::TypeParam(_, name)
            | ImplTargetKey::Builtin(name) => Some(name),
            ImplTargetKey::Ref(_) => None,
        }
    }

    /// How to spell this target in a diagnostic — the declaration name, or the
    /// reference prefix for a `&T` / `&mut T` target.
    pub(crate) fn display_name(&self) -> &str {
        match self {
            ImplTargetKey::Decl((_, name))
            | ImplTargetKey::TypeParam(_, name)
            | ImplTargetKey::Builtin(name) => name,
            ImplTargetKey::Ref(kind) => kind.prefix(),
        }
    }
}

pub(super) type TraitImplIndex = IndexMap<ImplTargetKey, Vec<(ModuleSource, AstId)>>;

type ReceiverImplIndex = IndexMap<name::Receiver, Vec<(ModuleSource, AstId)>>;

fn index_by_receiver(index: &TraitImplIndex) -> ReceiverImplIndex {
    let mut out: ReceiverImplIndex = IndexMap::default();
    for (key, entries) in index {
        out.entry(key.receiver())
            .or_default()
            .extend(entries.iter().cloned());
    }
    out
}

/// Digested header of an `impl` block, pre-extracted at [`TraitEnv::build`]
/// time so trait/method queries read its trait name, target type, methods,
/// and type parameters without re-fetching the impl block from
/// `loaded_modules`. Keyed by `(ModuleSource, AstId)` in
/// [`TraitEnv::impl_headers`].
#[derive(Clone, Debug)]
pub(super) struct ImplHeader {
    /// The module that wrote this header — the vantage every name in
    /// [`Self::ty`] and [`Self::trait_type`] is spelled from. Without it a
    /// consumer holding the header alone can only compare spellings, which is
    /// what makes two modules' same-named types look like one.
    pub(super) module: ModuleSource,
    /// Identity of the impl target, resolved once from [`Self::module`]'s
    /// vantage. The key every impl index in this file is keyed by, so a
    /// whole-program check compares identities rather than written heads.
    pub(super) target: ImplTargetKey,
    /// Identity of the implemented trait, resolved the same way; `None` for
    /// inherent `impl Type { … }` blocks.
    pub(super) trait_key: Option<ImplTargetKey>,
    /// What the trait reference in this header refers to, read from
    /// `Resolutions` rather than resolved a second time. This is what an impl
    /// index matches against, so a lookup compares declarations rather than
    /// spellings two modules can share (WEP 2026-08-10).
    pub(super) trait_ref: Option<crate::resolve::DeclRef>,
    /// Trait name for `impl Trait for Type` blocks (via `get_type_name_static`
    /// on the trait reference); `None` for inherent `impl Type { … }` blocks.
    /// The memoised head name of [`Self::trait_type`], so the index filters
    /// that only ask "is this a trait impl?" need no allocation.
    ///
    /// A spelling, not an identity — compare [`Self::trait_key`] instead when
    /// the question is *which* trait this is.
    pub(super) trait_name: Option<String>,
    /// The full trait reference (`Index<K>` in `impl Index<K> for Map`), for
    /// consumers that need its generic arguments rather than its head name.
    pub(super) trait_type: Option<Type>,
    /// The impl target type (`impl_block.ty`).
    pub(super) ty: Type,
    /// The impl block's type parameters.
    pub(super) type_params: Vec<ast::GenericParam>,
    /// Digested signatures of the block's methods, in source order. Carries
    /// only what method-lookup queries read off the AST today; extended as
    /// further consumers move onto the digest.
    pub(super) methods: Vec<ImplMethodHeader>,
    /// The block's `type X = …;` associated-type bindings, cloned so
    /// associated-type resolution reads them without the impl-block AST.
    pub(super) associated_types: Vec<ast::AssociatedTypeBinding>,
    /// `impl Trait for Type;` — a body-less derivation request rather than a
    /// real impl (WEP 2026-06-25 trait derivation).
    pub(super) is_synthesize_request: bool,
    /// Where the block is written, for diagnostics raised against it.
    pub(super) span: Span,
}

impl ImplHeader {
    /// The implemented trait as a mangled method name embeds it: named by the
    /// module that declares it, carrying the header's written type arguments.
    /// `None` for an inherent impl, and for a trait position filled by a
    /// binder or a name that reaches no declaration.
    pub(super) fn fq_trait(&self) -> Option<name::FqTraitName> {
        let written = get_type_name_full_static(self.trait_type.as_ref()?);
        match self.trait_key.as_ref()? {
            ImplTargetKey::Decl((module, name)) => Some(name::FqTraitName::declared_as_written(
                module, name, &written,
            )),
            ImplTargetKey::TypeParam(_, name) => Some(name::FqTraitName::binder(name)),
            ImplTargetKey::Ref(_) | ImplTargetKey::Builtin(_) => None,
        }
    }
}

/// The pack-bound associated types each blanket impl projects, keyed by the
/// blanket's `(module, ast_id)`.
///
/// The bound's own reference site says which trait declares the associated
/// type, so two modules' same-named bounds stay apart — the spelling the
/// blanket wrote cannot answer that.
fn blanket_pack_assocs(
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    blanket_impls: &IndexMap<String, Vec<BlanketImpl>>,
    resolutions: &crate::resolve::Resolutions,
) -> IndexMap<(ModuleSource, AstId), Vec<(DeclKey, String)>> {
    let mut out: IndexMap<(ModuleSource, AstId), Vec<(DeclKey, String)>> = IndexMap::default();
    for blanket in blanket_impls.values().flatten() {
        let key = (blanket.module.clone(), blanket.ast_id);
        let Some(header) = impl_headers.get(&key) else {
            continue;
        };
        let pairs: Vec<(DeclKey, String)> = header
            .type_params
            .iter()
            .flat_map(|tp| &tp.bounds)
            .flat_map(|bound| bound.assoc_types.iter().map(move |a| (bound, a)))
            .filter(|(_, assoc)| {
                matches!(&assoc.ty, ast::Type::Tuple(elems)
                    if elems
                        .iter()
                        .any(|e| matches!(e, ast::Type::TypePackSpread(..))))
            })
            .filter_map(|(bound, assoc)| {
                let (module, name) = resolutions.declared(bound.id)?;
                Some(((module.clone(), name.to_string()), assoc.name.clone()))
            })
            .collect();
        if !pairs.is_empty() {
            out.insert(key, pairs);
        }
    }
    out
}

/// What fixes one of a blanket impl's type parameters.
#[derive(Clone, Debug)]
pub(crate) enum BlanketParamSource {
    /// The impl's receiver, which the call site's receiver type fills.
    Receiver,
    /// A predicate on another parameter: `..F` in
    /// `impl<S: ReflectStruct<FieldTypes = [..F]>, ..F>`.
    Projection(DeclKey, String),
    /// A predicate names it, but the bound's site reaches no declaration.
    /// Its own answer: reading it as [`Self::Receiver`] would fill a pack from
    /// the call site's receiver type.
    Unresolved,
}

/// What determines each blanket impl's parameters, in declaration order, keyed
/// by the blanket's `(module, ast_id)`.
///
/// `None` is the receiver, which the call site's receiver type fills;
/// `Some((trait, associated type))` is a parameter a predicate fixes — `..F` in
/// `impl<S: ReflectStruct<FieldTypes = [..F]>, ..F>`. Declaration order is the
/// point: the impl's type arguments are consumed positionally, and a receiver
/// written after another parameter sits at a slot that "receiver first,
/// projections after" never fills.
///
/// The bound is keyed by its own reference site, like [`blanket_pack_assocs`],
/// so the trait it names is the declaration rather than the spelling.
fn blanket_param_sources(
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    blanket_impls: &IndexMap<String, Vec<BlanketImpl>>,
    resolutions: &crate::resolve::Resolutions,
) -> IndexMap<(ModuleSource, AstId), Vec<BlanketParamSource>> {
    let mut out: IndexMap<(ModuleSource, AstId), Vec<BlanketParamSource>> = IndexMap::default();
    for blanket in blanket_impls.values().flatten() {
        let key = (blanket.module.clone(), blanket.ast_id);
        let Some(header) = impl_headers.get(&key) else {
            continue;
        };
        let sources: Vec<BlanketParamSource> = header
            .type_params
            .iter()
            .filter(|tp| tp.is_real_type_param())
            .map(|tp| {
                if tp.name == blanket.param {
                    return BlanketParamSource::Receiver;
                }
                let Some((bound, assoc)) = header
                    .type_params
                    .iter()
                    .flat_map(|other| &other.bounds)
                    .flat_map(|bound| bound.assoc_types.iter().map(move |a| (bound, a)))
                    .find(|(_, assoc)| {
                        let mut named = Vec::new();
                        assoc.ty.mentioned_names(&mut named);
                        named.iter().any(|n| n == &tp.name)
                    })
                else {
                    return BlanketParamSource::Unresolved;
                };
                let Some((module, name)) = resolutions.declared(bound.id) else {
                    return BlanketParamSource::Unresolved;
                };
                BlanketParamSource::Projection(
                    (module.clone(), name.to_string()),
                    assoc.name.clone(),
                )
            })
            .collect();
        out.insert(key, sources);
    }
    out
}

/// Digested signature of a single method inside an [`ImplHeader`]. Holds the
/// name and type parameters method-lookup queries need without the method
/// body; grows field-by-field as consumers migrate off the impl-block AST.
#[derive(Clone, Debug)]
pub(super) struct ImplMethodHeader {
    pub(super) name: String,
    /// The method's own `AstId` — the key into the canonical-signature
    /// digest, so a header lookup reaches the signature without the AST.
    pub(super) ast_id: AstId,
    pub(super) type_params: Vec<ast::GenericParam>,
    /// Where the method is written, so a whole-program check reporting on it
    /// needs no second walk of the module AST to find the span.
    pub(super) span: Span,
    /// The span of the method's name alone, for a diagnostic that points at
    /// the signature rather than the whole body.
    pub(super) name_span: Span,
    /// Parameter count excluding `self`, so an arity check reads the digest
    /// instead of the method AST.
    pub(super) param_count: usize,
}

/// The receiver shape of a blanket impl.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BlanketReceiver {
    /// `impl<T: Bound> Trait for T` — applies to any value receiver.
    Value,
    /// `impl<T: Bound> Trait for &T` (`is_mut` selects `&mut T`) — applies to
    /// any reference receiver.
    Ref { is_mut: bool },
}

/// A bound written on a blanket impl's receiver parameter.
#[derive(Clone, Debug)]
pub(crate) struct BlanketBound {
    pub(crate) name: String,
    /// What the bound's reference site resolves to, `None` where it reaches no
    /// declaration.
    pub(crate) decl_ref: Option<crate::resolve::DeclRef>,
}

/// A reified blanket impl `impl<Param: Bounds, ..> Trait for <receiver>`.
///
/// The single source of truth for "what kind of blanket is this": the queries
/// that once re-derived it per call site are now selections over this
/// descriptor.
#[derive(Clone, Debug)]
pub(crate) struct BlanketImpl {
    pub(crate) module: ModuleSource,
    /// The impl block's AST id; with `module`, the key into `impl_headers` for
    /// consumers needing the full header (associated types, bound constraints).
    pub(crate) ast_id: AstId,
    pub(crate) receiver: BlanketReceiver,
    /// Receiver param name (`T` in `impl<T: Bound> Trait for T`).
    pub(crate) param: String,
    /// Bound trait names on the receiver param, each with the declaration its
    /// own reference site resolves to. The spelling stays for the by-name
    /// queries that have not been flipped; the answer is what a bound check
    /// compares, so an aliased bound reaches the trait it aliases.
    pub(crate) bounds: Vec<BlanketBound>,
}

/// Classify a blanket impl's receiver, or `None` for a concrete/shape impl
/// (`impl Display for String`, `impl<T> IntoIterator for &List<T>`). A blanket
/// receiver is a *bounded* type param (`impl<T: B> Trait for T`) or a reference
/// to a type param (`impl<T: B> Trait for &T`). Returns the receiver kind and
/// the param name.
fn classify_blanket_receiver(
    ty: &ast::Type,
    type_params: &[ast::GenericParam],
) -> Option<(BlanketReceiver, String)> {
    let is_param = |name: &str| type_params.iter().any(|p| p.name == name);
    let is_bounded_param = |name: &str| {
        type_params
            .iter()
            .any(|p| p.name == name && !p.bounds.is_empty())
    };
    match ty {
        Type::Named(named) if is_bounded_param(&named.name) => {
            Some((BlanketReceiver::Value, named.name.clone()))
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            let is_mut = matches!(ty, Type::MutReference(_));
            match inner.as_ref() {
                Type::Named(named) if is_param(&named.name) => {
                    Some((BlanketReceiver::Ref { is_mut }, named.name.clone()))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Digested header of a `trait` declaration: its name plus per-method
/// signatures method-lookup queries read off the AST. Built in
/// [`TraitEnv::build`] and keyed by `(ModuleSource, AstId)` in
/// [`TraitEnv::trait_decl_headers`]. Reuses [`ImplMethodHeader`] for the
/// per-method digest (name + type parameters).
#[derive(Clone, Debug)]
pub(super) struct TraitDeclHeader {
    pub(super) name: String,
    /// The trait's own type parameters (e.g. `<T, U>` in `trait Foo<T, U>`).
    pub(super) type_params: Vec<ast::GenericParam>,
    /// Direct supertraits as written (`trait Ord: Eq`). The transitive form
    /// lives in [`TraitEnv::supertrait_closure`].
    pub(super) supertraits: Vec<ast::TraitBound>,
    pub(super) methods: Vec<ImplMethodHeader>,
    /// The trait's `type X: Bounds;` declarations, in order. Here rather than
    /// in the `TypeId`-level digest because the decl pass asks which trait
    /// declares `Self::X` while resolving that trait's own method signatures,
    /// before any digest exists.
    pub(super) assoc_types: Vec<ast::AssociatedTypeDecl>,
    pub(super) span: Span,
}

/// Pre-built index: `(declaring module, trait name)` → (`ModuleSource`, `AstId`)
/// for trait declarations.
pub(super) type TraitDeclIndex = IndexMap<DeclKey, (ModuleSource, AstId)>;

/// A trait declaration's location, the key both [`TraitEnv::trait_decl_headers`]
/// and [`SupertraitClosureIndex`] use.
type TraitDeclLoc = (ModuleSource, AstId);

/// A supertrait paired with the declaration it resolved to. The bound keeps the
/// declaring module's spelling, which need not name the same trait elsewhere.
#[derive(Clone, Debug)]
pub(super) struct InheritedBound {
    pub(super) bound: ast::TraitBound,
    pub(super) decl: DeclKey,
}

/// Pre-built index: trait declaration → the transitive closure of its
/// supertraits, deduplicated by declaration and excluding the trait itself. A
/// declared bound `T: Sub` expands through this so `T: Ord` alone carries
/// `Eq`.
pub(super) type SupertraitClosureIndex = IndexMap<TraitDeclLoc, Vec<InheritedBound>>;

/// Pre-built index: `(declaring module, effect name)` → (`ModuleSource`,
/// `AstId`) for effect declarations. Effects are first-class citizens distinct
/// from traits and have their own impl form (`impl Effect for Type`)
/// interpreted as installable handlers, so the elaborator and dispatch
/// synthesis need to distinguish them quickly.
pub(super) type EffectDeclIndex = IndexMap<DeclKey, (ModuleSource, AstId)>;

/// Pre-built index: `(declaring module, resource name)` → (`ModuleSource`,
/// `AstId`) for resource declarations. Resources participate in
/// `with R => h do` / `impl R for Type` exactly like effects (see WEP
/// 2026-04-11): both kinds of declaration carry a list of operations that
/// user handler implementations satisfy and that the dispatch-synthesis
/// pass routes through wrappers. Indexed separately from effects so the
/// elaborator can keep diagnostics ("not an effect", "not a resource")
/// truthful and so the dispatch synthesis can know not to declare the
/// resource on its wrapper's `effects` list (resources are not effects).
pub(super) type ResourceDeclIndex = IndexMap<DeclKey, (ModuleSource, AstId)>;

/// Pre-built index of static methods (no `self` parameter) from impl blocks.
/// Key: canonical receiver [`DeclKey`] → list of
/// A static (receiver-less) method reachable by name on a type.
#[derive(Clone, Debug)]
pub(super) struct StaticMethodEntry {
    pub(super) name: String,
    /// The method itself: the key into the signature digest, which carries
    /// everything a lookup needs — resolved in the impl's own frame and its
    /// own module's perspective.
    pub(super) method_id: AstId,
}

/// Pre-built index of static methods, for O(1) lookup instead of a scan over
/// every module.
///
/// Keyed by canonical declaration key rather than bare type name, so
/// `impl Counter { fn make(...) }` in two modules with same-named
/// `struct Counter` produces two buckets and `CounterA::make(...)` reaches the
/// right one.
pub(super) type StaticMethodIndex = IndexMap<DeclKey, Vec<StaticMethodEntry>>;

/// Pre-built index of static methods from resource declarations.
/// Key: canonical receiver [`DeclKey`] → `[(method_name, ModuleSource,
/// item_ast_id, method_index)]`. Same disambiguation rationale as
/// [`StaticMethodIndex`].
pub(super) type ResourceStaticMethodIndex =
    IndexMap<DeclKey, Vec<(String, ModuleSource, AstId, usize)>>;

/// `(type_name, trait_name)` → modules whose `impl <trait_name> for <type_name>`
/// block exists.
///
/// Keyed by bare names (not [`DeclKey`]): the multi-value `Vec` plus the
/// caller's `type_module` hint already lets two modules' same-named
/// receivers each route to their own impl. Canonical disambiguation of
/// the receiver type's *declaring* module would require build-time import
/// resolution that the current `TraitEnv::build` doesn't have plumbed
/// through; a follow-up could re-key by canonical pair when the
/// inhabited-by-multiple-declarations case becomes user-visible.
///
/// Value blanket impls (`impl<T: Trait> Trait for T`) are represented by
/// [`BlanketImpl`] (in `blanket_impls`); they are excluded from this map because
/// they apply structurally and don't have a concrete receiver type name.
pub(crate) type TraitImplModuleIndex = IndexMap<(String, String), Vec<ModuleSource>>;

/// Where each `impl <trait> for <type>` lives, reachable from both receiver
/// namespaces.
///
/// The two are not interchangeable — a mangled head (`mod/Widget`) picks out
/// one declaration, a declared name (`Widget`) picks out any declaration
/// spelling itself that way — so they get separate storage and a query answers
/// only from the namespace it named. Storing both in one map is what let a
/// mangled query reach only the synthesised layer and a declared query only the
/// AST layer (WEP 2026-08-10).
#[derive(Debug, Default, Clone)]
pub struct ImplModuleIndex {
    by_mangled: TraitImplModuleIndex,
    by_declared: TraitImplModuleIndex,
}

impl ImplModuleIndex {
    fn get(&self, receiver: ImplReceiver<'_>, trait_name: &str) -> Option<&Vec<ModuleSource>> {
        let key = |spelling: String| (spelling, trait_name.to_string());
        match receiver {
            // One identity, so both namespaces are this receiver's own — no
            // choice to get wrong, and no reason to prefer either.
            ImplReceiver::Of(r) => self
                .by_mangled
                .get(&key(r.head_key().into_string()))
                .or_else(|| self.by_declared.get(&key(r.decl_key().into_string()))),
            ImplReceiver::Instantiated(m) => {
                self.by_mangled.get(&key(m.as_mangled_str().to_string()))
            }
            ImplReceiver::Declared(d) => self.by_declared.get(&key(d.as_decl_str().to_string())),
        }
    }

    /// Record `module` under both spellings of one receiver identity, so the
    /// two namespaces cannot drift apart.
    pub fn record(&mut self, receiver: &name::Receiver, trait_name: &str, module: &ModuleSource) {
        push_module(
            &mut self.by_mangled,
            receiver.head_key().into_string(),
            trait_name,
            module,
        );
        // A type parameter names no declaration, so it has no entry in the
        // declaration namespace. Giving it one lets a generic impl's own `T`
        // answer for a user `struct T` — the two are not the same receiver, and
        // only the mangled namespace keeps a binder scoped to its template.
        if !receiver.is_binder() {
            push_module(
                &mut self.by_declared,
                receiver.decl_key().into_string(),
                trait_name,
                module,
            );
        }
    }

    /// Record an impl on a generic head under its *instantiated* mangled
    /// receiver (`List<…/Token>`), distinct from the bare head. Mangled-only:
    /// the declaration namespace has no spelling for an instantiation.
    pub fn record_instantiated(
        &mut self,
        mangled: String,
        trait_name: &str,
        module: &ModuleSource,
    ) {
        push_module(&mut self.by_mangled, mangled, trait_name, module);
    }
}

fn push_module(
    map: &mut TraitImplModuleIndex,
    receiver: String,
    trait_name: &str,
    module: &ModuleSource,
) {
    let modules = map.entry((receiver, trait_name.to_string())).or_default();
    if !modules.contains(module) {
        modules.push(module.clone());
    }
}

/// Where every non-blanket `impl` block lives, in both receiver namespaces,
/// read off the headers' resolved identities rather than the heads they wrote.
///
/// `concrete_only` keeps just the impl blocks with no type parameters: the
/// monomorphizer redirects a substituted call to a concrete impl's own module,
/// while a generic impl's instance is materialised in the receiver type's
/// module by convention.
///
/// A value blanket (`impl<T: Bound> Trait for T`) has no per-type home, and its
/// target keys as [`ImplTargetKey::TypeParam`], so excluding that variant is
/// what leaves it out.
fn index_impl_modules(
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    concrete_only: bool,
) -> ImplModuleIndex {
    let mut out = ImplModuleIndex::default();
    for header in impl_headers.values() {
        // A bodiless derive (`impl Deserialize for Point;`) asks for an impl,
        // it does not host one. This index answers "which module holds the
        // code", and answering with the request sends a type-param dispatch to
        // a module with no body — where it would otherwise have reached the
        // blanket that serves it. The generated body registers itself in the
        // synthesis layer, under the module it actually landed in.
        if header.is_synthesize_request {
            continue;
        }
        if matches!(header.target, ImplTargetKey::TypeParam(..)) {
            continue;
        }
        if concrete_only && !header.type_params.is_empty() {
            continue;
        }
        let Some(fq_trait) = header.fq_trait() else {
            continue;
        };
        out.record(
            &header.target.receiver(),
            fq_trait.base_name(),
            &header.module,
        );
    }
    out
}

/// Immutable global knowledge base for trait resolution.
///
/// Contains pre-built indices for fast lookup of trait implementations,
/// trait declarations, and blanket impls. Built once before resolution
/// begins and shared (via `Arc`) across all module elaborators.
/// `TraitEnv` is *intentionally* not `Clone`. After `build()` returns, the
/// only legitimate way to mutate the env is `extend_with_synthesised`,
/// which moves out of an `Arc` whose strong count must be 1. Forbidding
/// clones at the type level surfaces accidental `Arc` sharing as a
/// compile error rather than a silent deep-clone of every index.
#[derive(Debug)]
pub struct TraitEnv {
    /// Type name → impl blocks that implement traits for that type.
    pub(super) impl_index: TraitImplIndex,
    /// Type name → **every** impl block (inherent and trait) on that type, in
    /// global build order (matching `impl_headers`'s insertion order), so
    /// candidate scans iterate directly with no per-call sort. Keyed like
    /// `impl_index` (bare name via `get_type_name_static`); same-named types in
    /// different modules share a bucket, disambiguated by the per-entry
    /// `ModuleSource`. The inherent subset is the `trait_name.is_none()` filter
    /// ([`Self::inherent_impl_keys`]).
    pub(super) all_impl_index: TraitImplIndex,
    /// `impl_index` and `all_impl_index` re-keyed by the target's bare head,
    /// for callers that hold a name without the import context to canonicalise
    /// it. Built once with the indexes it mirrors: derived per query it is a
    /// scan of every impl target, and bound checking during method lookup runs
    /// on that path.
    by_receiver: ReceiverImplIndex,
    all_by_receiver: ReceiverImplIndex,
    /// Trait name → trait declaration location.
    pub(super) decl_index: TraitDeclIndex,
    /// Effect name → effect declaration location.
    pub(super) effect_decl_index: EffectDeclIndex,
    /// Resource name → resource declaration location. Used alongside
    /// `effect_decl_index` to recognise handler-installable kinds in `with`
    /// clauses and `impl R for T` blocks.
    pub(super) resource_decl_index: ResourceDeclIndex,
    /// Digested headers for every indexed impl block, keyed by
    /// `(ModuleSource, AstId)`. Trait/method queries read this instead of
    /// re-fetching the impl block AST from `loaded_modules`. See [`ImplHeader`].
    pub(super) impl_headers: IndexMap<(ModuleSource, AstId), ImplHeader>,
    /// Per blanket impl, the `(declaring trait, associated type)` pairs whose
    /// binding is a type pack. Resolved once at build time from each bound's
    /// own reference site, so the trait is a declaration rather than the
    /// spelling the blanket wrote (WEP 2026-08-10).
    pub(super) blanket_pack_assocs: IndexMap<(ModuleSource, AstId), Vec<(DeclKey, String)>>,
    /// Per blanket impl, what determines each of its parameters, in
    /// declaration order. Resolved once at build time from each bound's own
    /// reference site.
    pub(super) blanket_param_sources: IndexMap<(ModuleSource, AstId), Vec<BlanketParamSource>>,
    /// Digested headers for every `trait` declaration, keyed by
    /// `(ModuleSource, AstId)`. Lets method-lookup queries read trait
    /// method signatures without re-fetching the trait AST. See
    /// [`TraitDeclHeader`].
    pub(super) trait_decl_headers: IndexMap<TraitDeclLoc, TraitDeclHeader>,
    /// Transitive supertraits per trait declaration. See
    /// [`SupertraitClosureIndex`].
    supertrait_closures: SupertraitClosureIndex,
    /// The same closures keyed by bare trait name, for names declared exactly
    /// once. Prelude-implicit names (`Ord`, `Eq`, …) reach a query through no
    /// import, so a scoped lookup cannot canonicalise them.
    supertrait_closures_by_name: IndexMap<String, Vec<InheritedBound>>,
    /// Free-function type parameters keyed by `(declaring module, function
    /// name)`. Lets `lookup_function_type_params` read a callee's type params
    /// without scanning the module AST.
    pub(super) function_type_params: IndexMap<(ModuleSource, String), Vec<ast::GenericParam>>,
    /// Type name → modules declaring a struct / resource / variant / enum /
    /// builtin type of that name, in build order. Powers
    /// `find_struct_module_source` without an AST scan. Newtypes are tracked
    /// separately in [`Self::newtype_decl_modules`] because the query consults
    /// them only as a later fallback.
    pub(super) struct_like_decl_modules: IndexMap<String, Vec<ModuleSource>>,
    /// Type name → modules declaring a `newtype` of that name, in build order.
    /// The fallback half of `find_struct_module_source`'s module lookup.
    pub(super) newtype_decl_modules: IndexMap<String, Vec<ModuleSource>>,
    /// Per-module import scope (`use`-derived `imported_type_sources` /
    /// `import_original_names`), pre-computed once. Dispatch-core queries that
    /// resolve type names in a foreign impl/callee signature read this instead
    /// of rebuilding it from the module AST in `loaded_modules`. See
    /// [`module_import_scope`].
    pub(super) module_import_scopes: IndexMap<ModuleSource, ModuleImportScope>,
    /// Associated-type name → the declaring trait's bounds for it, first
    /// declaration wins (matching the previous whole-program scan order).
    /// Consumed by `find_assoc_type_bounds` without an AST scan.
    pub(super) assoc_type_bound_index: IndexMap<String, Vec<ast::TraitBound>>,
    /// `trait_name` → reified blanket impls of that trait, in registration
    /// order. The single classification source for blanket dispatch (module,
    /// receiver kind, param, bounds); the `blanket_impl_*_for_trait` queries
    /// select over it. Used by the monomorphizer to find the home module of a
    /// generic dispatch when the receiver type has no dedicated `impl Trait for
    /// Type` block — the blanket provides the body, homed in the blanket's
    /// module. Keyed by bare trait name; the `type_module` hint at the call site
    /// disambiguates when several modules host a blanket for the same trait.
    pub(super) blanket_impls: IndexMap<String, Vec<BlanketImpl>>,
    /// `type_name` → `[(method_name, ModuleSource, item_ast_id, method_idx)]` for static methods.
    pub(super) static_method_index: StaticMethodIndex,
    /// `type_name` → `[(method_name, ModuleSource, item_ast_id, method_idx)]` for resource static methods.
    pub(super) resource_static_method_index: ResourceStaticMethodIndex,
    /// Where every non-blanket AST-level `impl` block lives, in both receiver
    /// namespaces. Built from the impl headers' resolved identities, so an
    /// entry names the declaration the header meant rather than the head it
    /// wrote.
    trait_impl_modules: ImplModuleIndex,
    /// The concrete-only subset of [`Self::trait_impl_modules`] — impl blocks
    /// with no type parameters. See [`Self::concrete_impl_module_for`].
    concrete_trait_impl_modules: ImplModuleIndex,
    /// Layer added in the synthesis phase: auto-derived / generated impls
    /// (`Eq`, `Ord`, `Inspect`, `Display`, `From`, serde adapters, …) that
    /// were not present in the AST. `None` until `extend_with_synthesised`
    /// runs (e.g. on the LSP path, which never reaches synthesis). Once
    /// populated, the field is itself immutable; later phases either query
    /// it or replace the whole `TraitEnv` with a further-extended copy.
    pub(crate) synthesised: Option<SynthesisedImpls>,
}

/// Trait impls produced by the synthesis phase but not present in the AST.
/// Populated by [`TraitEnv::extend_with_synthesised`].
#[derive(Debug, Default, Clone)]
pub struct SynthesisedImpls {
    /// Where each synthesized non-blanket trait impl lives (auto-derives plus
    /// the impls produced by `from_synth` / `serde_synth`). Same shape as the
    /// AST layer and consulted through the same [`TraitEnv::impl_module_for`].
    /// Includes both concrete (e.g. auto-derived `Inspect for Wrapper`) and
    /// generic synthesised impls.
    pub trait_impl_modules: ImplModuleIndex,
    /// Concrete-only subset (no impl-block type parameters). See
    /// [`TraitEnv::concrete_impl_module_for`] for why mono needs to
    /// distinguish concrete impls from generic ones.
    pub concrete_trait_impl_modules: ImplModuleIndex,
}

impl SynthesisedImpls {
    /// Record that `impl <trait_name> for <type_name>` has been synthesized
    /// in `module`. `is_concrete` indicates whether the impl has no
    /// generic type parameters, so it can be added to the concrete-only
    /// view. Each `module` is recorded at most once per key; iteration
    /// order matches insertion order so callers can rely on a stable
    /// "first registered" fallback when no `type_module` hint is supplied.
    pub fn record_impl(
        &mut self,
        receiver: &name::Receiver,
        trait_name: &str,
        module: &ModuleSource,
        is_concrete: bool,
    ) {
        if is_concrete {
            self.concrete_trait_impl_modules
                .record(receiver, trait_name, module);
        }
        self.trait_impl_modules.record(receiver, trait_name, module);
    }

    /// Record a concrete impl on a generic head (`impl Tag for List<Token>`)
    /// under its instantiated receiver, so it does not collide with another
    /// module's `impl Tag for List<OtherToken>` on the shared head (#1348).
    pub fn record_instantiation(
        &mut self,
        mangled: String,
        trait_name: &str,
        module: &ModuleSource,
    ) {
        self.concrete_trait_impl_modules
            .record_instantiated(mangled.clone(), trait_name, module);
        self.trait_impl_modules
            .record_instantiated(mangled, trait_name, module);
    }
}

impl TraitEnv {
    /// Build trait indices from all loaded modules.
    ///
    /// Called once in [`Elaborator::annotate_modules`] before per-module
    /// resolution begins.
    /// The indices enable O(1) trait lookup by type/trait name instead of scanning all modules.
    /// Also performs orphan rule checking for impl blocks in local (user) modules.
    ///
    /// `symbols` is consulted via [`SymbolTable::lookup_in_module`] to
    /// canonicalise every receiver-type and trait-name reference that
    /// appears in an `impl` block: bare-name lookups are resolved against
    /// the impl module's own import context, so two modules with same-
    /// named traits / structs each produce a distinct [`DeclKey`] in the
    /// affected indices. The symbol table is built by the `analyze` phase,
    /// which runs before this routine.
    pub(super) fn build(
        modules: &IndexMap<ModuleSource, Module>,
        symbols: &SymbolTable,
        interner: &mut ModuleSourceInterner,
        entry_module: Option<&ModuleSource>,
        invocations: &InvocationIndex,
        resolutions: &crate::resolve::Resolutions,
    ) -> (Arc<Self>, Vec<(ModuleSource, TypeError)>) {
        // Pre-compute every module's `use`-derived import scope so dispatch
        // queries read it instead of rebuilding from the module AST.
        let mut module_import_scopes: IndexMap<ModuleSource, ModuleImportScope> =
            IndexMap::default();
        for (module_source, module) in modules {
            module_import_scopes.insert(
                module_source.clone(),
                module_import_scope(
                    interner,
                    module,
                    module_source,
                    entry_module,
                    invocations,
                    symbols,
                ),
            );
        }
        let mut impl_index: TraitImplIndex = IndexMap::default();
        let mut all_impl_index: TraitImplIndex = IndexMap::default();
        let mut decl_index: TraitDeclIndex = IndexMap::default();
        let mut effect_decl_index: EffectDeclIndex = IndexMap::default();
        let mut assoc_type_bound_index: IndexMap<String, Vec<ast::TraitBound>> =
            IndexMap::default();
        let mut resource_decl_index: ResourceDeclIndex = IndexMap::default();
        let mut blanket_impls: IndexMap<String, Vec<BlanketImpl>> = IndexMap::default();
        let mut impl_headers: IndexMap<(ModuleSource, AstId), ImplHeader> = IndexMap::default();
        let mut trait_decl_headers: IndexMap<(ModuleSource, AstId), TraitDeclHeader> =
            IndexMap::default();
        let mut function_type_params: IndexMap<(ModuleSource, String), Vec<ast::GenericParam>> =
            IndexMap::default();
        let mut struct_like_decl_modules: IndexMap<String, Vec<ModuleSource>> = IndexMap::default();
        let mut newtype_decl_modules: IndexMap<String, Vec<ModuleSource>> = IndexMap::default();
        // (declaring module, type name) → module source, for orphan rule
        // "is this type local?" checks. Keyed by canonical decl key so
        // two modules can declare a same-named type without colliding.
        let mut type_decl_index: IndexMap<DeclKey, ModuleSource> = IndexMap::default();

        let mut static_method_index: StaticMethodIndex = IndexMap::default();
        let mut resource_static_method_index: ResourceStaticMethodIndex = IndexMap::default();

        // Pass 1: walk every module's items to populate the
        // declaration-side indices (trait / effect / resource / type
        // decls). We need these populated *before* impl blocks are
        // canonicalised in pass 2, because the build-time canonical-key
        // helper falls back to scanning the decl indices when the
        // per-module symbol table misses (typical for prelude-implicit
        // names that no `use` declaration explicitly threads).
        for (module_source, module) in modules {
            for item in &module.items {
                match item {
                    Item::Trait(trait_decl) => {
                        // `(module_source, name)` key: two modules can declare
                        // a same-named trait without colliding. The previous
                        // bare-name key first-wrote-wins and silently routed
                        // both declarations to the same entry.
                        decl_index.insert(
                            (module_source.clone(), trait_decl.name.clone()),
                            (module_source.clone(), trait_decl.id),
                        );
                        for assoc in &trait_decl.associated_types {
                            assoc_type_bound_index
                                .entry(assoc.name.clone())
                                .or_insert_with(|| assoc.bounds.clone());
                        }
                    }
                    Item::Interface(effect_decl) => {
                        effect_decl_index.insert(
                            (module_source.clone(), effect_decl.name.clone()),
                            (module_source.clone(), effect_decl.id),
                        );
                    }
                    Item::Resource(resource) => {
                        let resource_key = (module_source.clone(), resource.name.clone());
                        resource_decl_index
                            .insert(resource_key.clone(), (module_source.clone(), resource.id));
                        // Index static methods from resource declarations.
                        // The resource declaration itself is the canonical
                        // receiver, so key by the declaration's own
                        // `(module, name)` pair.
                        for (method_idx, method) in resource.methods.iter().enumerate() {
                            let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name)
                            });
                            if !has_self {
                                resource_static_method_index
                                    .entry(resource_key.clone())
                                    .or_default()
                                    .push((
                                        method.name.clone(),
                                        module_source.clone(),
                                        resource.id,
                                        method_idx,
                                    ));
                            }
                        }
                    }
                    Item::Struct(s) => {
                        type_decl_index.insert(
                            (module_source.clone(), s.name.clone()),
                            module_source.clone(),
                        );
                    }
                    Item::Variant(v) => {
                        type_decl_index.insert(
                            (module_source.clone(), v.name.clone()),
                            module_source.clone(),
                        );
                    }
                    Item::Enum(e) => {
                        type_decl_index.insert(
                            (module_source.clone(), e.name.clone()),
                            module_source.clone(),
                        );
                    }
                    Item::Flags(f) => {
                        type_decl_index.insert(
                            (module_source.clone(), f.name.clone()),
                            module_source.clone(),
                        );
                    }
                    Item::Newtype(n) => {
                        type_decl_index.insert(
                            (module_source.clone(), n.name.clone()),
                            module_source.clone(),
                        );
                    }
                    Item::BuiltinTypeDecl(d) => {
                        type_decl_index.insert(
                            (module_source.clone(), d.name.clone()),
                            module_source.clone(),
                        );
                    }
                    Item::TupleTypeDecl(_) => {
                        type_decl_index.insert(
                            (
                                module_source.clone(),
                                TypeTable::TUPLE_TYPE_NAME.to_string(),
                            ),
                            module_source.clone(),
                        );
                    }
                    _ => {}
                }
            }
        }

        // Pass 2: walk impl blocks now that all decl indices are
        // populated, so the per-impl canonicalisation above can resolve
        // every PascalCase reference to its declaring module.
        for (module_source, module) in modules {
            for item in &module.items {
                // Digest the per-item facts that `lookup_function_type_params`
                // and `find_struct_module_source` read, so neither needs to
                // re-scan `loaded_modules`. (Non-impl items fall through to the
                // `Item::Impl` guard below and `continue`.)
                match item {
                    Item::Function(f) => {
                        function_type_params.insert(
                            (module_source.clone(), f.name.clone()),
                            f.type_params.clone(),
                        );
                    }
                    Item::Struct(s) => struct_like_decl_modules
                        .entry(s.name.clone())
                        .or_default()
                        .push(module_source.clone()),
                    Item::Resource(r) => struct_like_decl_modules
                        .entry(r.name.clone())
                        .or_default()
                        .push(module_source.clone()),
                    Item::Variant(v) => struct_like_decl_modules
                        .entry(v.name.clone())
                        .or_default()
                        .push(module_source.clone()),
                    Item::Enum(e) => struct_like_decl_modules
                        .entry(e.name.clone())
                        .or_default()
                        .push(module_source.clone()),
                    Item::BuiltinTypeDecl(d) => struct_like_decl_modules
                        .entry(d.name.clone())
                        .or_default()
                        .push(module_source.clone()),
                    Item::Newtype(n) => newtype_decl_modules
                        .entry(n.name.clone())
                        .or_default()
                        .push(module_source.clone()),
                    _ => {}
                }
                if let Item::Trait(trait_decl) = item {
                    trait_decl_headers.insert(
                        (module_source.clone(), trait_decl.id),
                        TraitDeclHeader {
                            name: trait_decl.name.clone(),
                            type_params: trait_decl.type_params.clone(),
                            supertraits: trait_decl.supertraits.clone(),
                            methods: trait_decl
                                .methods
                                .iter()
                                .map(|m| ImplMethodHeader {
                                    name: m.name.clone(),
                                    ast_id: m.id,
                                    type_params: m.type_params.clone(),
                                    span: m.span,
                                    name_span: m.name_span,
                                    param_count: m
                                        .params
                                        .iter()
                                        .filter(|p| p.self_kind == ast::SelfKind::None)
                                        .count(),
                                })
                                .collect(),
                            assoc_types: trait_decl.associated_types.clone(),
                            span: trait_decl.span,
                        },
                    );
                    continue;
                }
                let Item::Impl(impl_block) = item else {
                    continue;
                };
                let type_name = get_type_name_static(&impl_block.ty);
                let type_key =
                    impl_target_key_at(&impl_block.ty, module_source, symbols, resolutions);
                let trait_ref = impl_block
                    .trait_type
                    .as_ref()
                    .and_then(crate::resolve::head_site)
                    .and_then(|site| resolutions.get(site));
                let trait_key = impl_block.trait_type.as_ref().map(|trait_type| {
                    // The site the header wrote answers first. `impl_target_key`
                    // resolves through the symbol table, which holds no entry
                    // for a trait, so a module implementing its own `trait Sub`
                    // fell through to `core:prelude`'s arithmetic one.
                    trait_ref
                        .and_then(|answer| resolutions.decl_named(answer))
                        .map(|(module, name)| {
                            ImplTargetKey::Decl((module.clone(), name.to_string()))
                        })
                        .unwrap_or_else(|| {
                            // A trait position whose site names no declaration
                            // — a bodiless derive naming a stdlib trait the
                            // module never `use`d. The declaration indexes are
                            // the only thing that can answer, and they decline
                            // when several modules declare the name. Same chain
                            // as `Elaborator::decl_key_or_local`, so the header
                            // and the elaborator key the trait identically.
                            unique_declared_trait(
                                &get_type_name_static(trait_type),
                                &decl_index,
                                &effect_decl_index,
                                &resource_decl_index,
                            )
                            .map_or_else(
                                || {
                                    impl_target_key_at(
                                        trait_type,
                                        module_source,
                                        symbols,
                                        resolutions,
                                    )
                                },
                                ImplTargetKey::Decl,
                            )
                        })
                });
                impl_headers.insert(
                    (module_source.clone(), impl_block.id),
                    ImplHeader {
                        module: module_source.clone(),
                        target: type_key.clone(),
                        trait_key,
                        trait_ref,
                        trait_name: impl_block.trait_type.as_ref().map(get_type_name_static),
                        trait_type: impl_block.trait_type.clone(),
                        ty: impl_block.ty.clone(),
                        type_params: impl_block.type_params.clone(),
                        methods: impl_block
                            .methods
                            .iter()
                            .map(|m| ImplMethodHeader {
                                name: m.name.clone(),
                                ast_id: m.id,
                                type_params: m.type_params.clone(),
                                span: m.span,
                                name_span: m.name_span,
                                param_count: m
                                    .params
                                    .iter()
                                    .filter(|p| p.self_kind == ast::SelfKind::None)
                                    .count(),
                            })
                            .collect(),
                        associated_types: impl_block.associated_types.clone(),
                        is_synthesize_request: impl_block.is_synthesize_request,
                        span: impl_block.span,
                    },
                );
                // Joins `all_impl_index` before the trait/inherent split, so its
                // order matches `impl_headers`'s global insertion order.
                all_impl_index
                    .entry(type_key.clone())
                    .or_default()
                    .push((module_source.clone(), impl_block.id));
                if let Some(trait_type) = &impl_block.trait_type {
                    let trait_name = get_type_name_static(trait_type);
                    if let Some((receiver, param)) =
                        classify_blanket_receiver(&impl_block.ty, &impl_block.type_params)
                    {
                        let bounds: Vec<BlanketBound> = impl_block
                            .type_params
                            .iter()
                            .find(|p| p.name == param)
                            .map(|p| {
                                p.bounds
                                    .iter()
                                    .map(|b| BlanketBound {
                                        name: b.name.clone(),
                                        decl_ref: resolutions.get(b.id),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        blanket_impls
                            .entry(trait_name.clone())
                            .or_default()
                            .push(BlanketImpl {
                                module: module_source.clone(),
                                ast_id: impl_block.id,
                                receiver,
                                param,
                                bounds,
                            });
                    }
                    impl_index
                        .entry(type_key.clone())
                        .or_default()
                        .push((module_source.clone(), impl_block.id));
                    // Static methods on trait impl blocks (no `self`
                    // parameter) join the same canonical bucket as
                    // inherent statics. `f64::from_bits` and friends in
                    // `core:prelude/int128.wado` flow through this path.
                    let recv_key = static_receiver_key(&type_key, || {
                        (module_source.clone(), type_name.clone())
                    });
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if !has_self {
                            static_method_index
                                .entry(recv_key.clone())
                                .or_default()
                                .push(StaticMethodEntry {
                                    name: method.name.clone(),
                                    method_id: method.id,
                                });
                        }
                    }
                } else {
                    // Inherent impl: already in `all_impl_index`; here only its
                    // static methods need the dedicated index.
                    let recv_key = static_receiver_key(&type_key, || {
                        (module_source.clone(), type_name.clone())
                    });
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if !has_self {
                            static_method_index
                                .entry(recv_key.clone())
                                .or_default()
                                .push(StaticMethodEntry {
                                    name: method.name.clone(),
                                    method_id: method.id,
                                });
                        }
                    }
                }
            }
        }

        // The one answer to "which declaration does this written name mean?",
        // from the writing module's vantage. Every whole-program check below
        // takes it rather than reading a head off the AST, so no check can
        // fall back to comparing spellings.
        let resolve_written =
            |module: &ModuleSource, ty: &ast::Type, _type_params: &[ast::GenericParam]| {
                impl_target_key_at(ty, module, symbols, resolutions)
            };

        let mut violations = check_all_orphan_rules(
            &impl_headers,
            &decl_index,
            &type_decl_index,
            &resolve_written,
        );

        // The bound's own site says which trait it names, so an aliased
        // supertrait (`use { Base as B }; trait Extra: B`) keys on `Base`'s
        // declaration without the import scope being consulted a second time.
        let resolve_trait = |module: &ModuleSource, bound: &ast::TraitBound| {
            let key = resolutions.declared(bound.id).map_or_else(
                || (module.clone(), bound.name.clone()),
                |(decl_module, name)| (decl_module.clone(), name.to_string()),
            );
            decl_index.get(&key).cloned()
        };
        let trait_impl_modules = index_impl_modules(&impl_headers, false);
        let concrete_trait_impl_modules = index_impl_modules(&impl_headers, true);

        violations.extend(check_variadic_impl_overlap(&impl_headers));
        violations.extend(check_inherent_impl_collisions(&impl_headers));

        let (supertrait_closures, cycles) =
            build_supertrait_closures(&trait_decl_headers, &resolve_trait);
        violations.extend(cycles);

        (
            Arc::new(Self {
                by_receiver: index_by_receiver(&impl_index),
                all_by_receiver: index_by_receiver(&all_impl_index),
                impl_index,
                all_impl_index,
                decl_index,
                effect_decl_index,
                resource_decl_index,
                blanket_pack_assocs: blanket_pack_assocs(
                    &impl_headers,
                    &blanket_impls,
                    resolutions,
                ),
                blanket_param_sources: blanket_param_sources(
                    &impl_headers,
                    &blanket_impls,
                    resolutions,
                ),
                impl_headers,
                supertrait_closures_by_name: index_closures_by_name(
                    &trait_decl_headers,
                    &supertrait_closures,
                ),
                trait_decl_headers,
                supertrait_closures,
                function_type_params,
                struct_like_decl_modules,
                newtype_decl_modules,
                module_import_scopes,
                assoc_type_bound_index,
                blanket_impls,
                static_method_index,
                resource_static_method_index,
                trait_impl_modules,
                concrete_trait_impl_modules,
                synthesised: None,
            }),
            violations,
        )
    }

    /// The pre-computed import scope for `module`, cloned for callers that
    /// install it via `with_module_perspective_for` (which takes the maps by
    /// value). Returns empty maps for a module with no recorded scope,
    /// matching the previous `…unwrap_or_default()` behaviour.
    pub(super) fn import_scope(&self, module: &ModuleSource) -> ModuleImportScope {
        self.module_import_scopes
            .get(module)
            .cloned()
            .unwrap_or_default()
    }

    /// The transitive supertraits of the trait `key` names, deduplicated by
    /// declaration and excluding the trait itself. Empty for a trait with no
    /// supertrait clause, and for a name that declares no trait.
    pub(super) fn supertrait_closure(&self, key: &DeclKey) -> &[InheritedBound] {
        self.decl_index
            .get(key)
            .and_then(|loc| self.supertrait_closures.get(loc))
            .map_or_else(|| self.supertrait_closure_named(&key.1), Vec::as_slice)
    }

    /// [`Self::supertrait_closure`] for a caller holding a bare name with no
    /// import context to canonicalise it. Empty when the name is declared by
    /// more than one module.
    pub(super) fn supertrait_closure_named(&self, name: &str) -> &[InheritedBound] {
        self.supertrait_closures_by_name
            .get(name)
            .map_or(&[], Vec::as_slice)
    }

    /// Keys of every impl block on `type_key`, in global build order —
    /// inherent and trait alike.
    pub(super) fn all_impl_keys(&self, type_key: &ImplTargetKey) -> Vec<(ModuleSource, AstId)> {
        self.all_impl_index
            .get(type_key)
            .cloned()
            .unwrap_or_default()
    }

    /// Keys of the **inherent** impls on `type_name`, in global build order —
    /// the `trait_name.is_none()` subset of [`Self::all_impl_index`]. Used by
    /// instance-method lookup, which must not treat trait impls as inherent.
    pub(super) fn inherent_impl_keys(
        &self,
        type_key: &ImplTargetKey,
    ) -> Vec<(ModuleSource, AstId)> {
        self.all_impl_index
            .get(type_key)
            .map(|keys| {
                keys.iter()
                    .filter(|key| {
                        self.impl_headers
                            .get(*key)
                            .is_some_and(|h| h.trait_name.is_none())
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The one trait declaration named `name`, when exactly one module
    /// declares it.
    ///
    /// For a reference the module's own scope cannot answer — a bodiless
    /// derive (`impl Deserialize for Point;`) naming a stdlib trait the module
    /// never `use`d. Declines when several modules declare the name: guessing
    /// between them is the mis-identification this design exists to prevent.
    pub(crate) fn unique_trait_decl_key(&self, name: &str) -> Option<DeclKey> {
        let mut hits = self.decl_index.keys().filter(|(_, n)| n == name);
        let first = hits.next()?;
        hits.next().is_none().then(|| first.clone())
    }

    /// The one effect or resource declaration named `name`, when exactly one
    /// module declares it. Declines on ambiguity, like
    /// [`Self::unique_trait_decl_key`].
    pub(crate) fn unique_effect_or_resource_decl_key(&self, name: &str) -> Option<DeclKey> {
        let mut hits = self
            .effect_decl_index
            .keys()
            .chain(self.resource_decl_index.keys())
            .filter(|(_, n)| n == name);
        let first = hits.next()?;
        hits.next().is_none().then(|| first.clone())
    }

    /// Whether `key` names a trait declaration.
    pub(crate) fn declares_trait(&self, key: &DeclKey) -> bool {
        self.decl_index.contains_key(key)
    }

    /// Whether `key` names an effect or a resource declaration.
    pub(crate) fn declares_effect_or_resource(&self, key: &DeclKey) -> bool {
        self.effect_decl_index.contains_key(key) || self.resource_decl_index.contains_key(key)
    }

    /// Declaring module of a struct-like type (struct / resource / variant /
    /// enum / builtin) by name, when the name picks out exactly one. Several
    /// modules declaring the name leaves it unresolved rather than guessing:
    /// a wrong module is worse than the caller's existing fallback.
    pub(crate) fn find_struct_like_decl_key(&self, name: &str) -> Option<DeclKey> {
        let modules = self.struct_like_decl_modules.get(name)?;
        match modules.as_slice() {
            [only] => Some((only.clone(), name.to_string())),
            _ => None,
        }
    }

    /// The module defining `impl <trait_name> for <receiver>`, or `None` for a
    /// blanket impl and for a receiver no concrete impl represents (an
    /// anonymous function type whose `Inspect` synthesis auto-derives
    /// per-module).
    ///
    /// The AST layer answers first: it holds the impls a module wrote, and the
    /// synthesis layer records both receiver namespaces, so preferring it would
    /// return a different module. When several modules implement the trait for
    /// same-named receivers, `type_module` picks the entry whose module matches.
    pub(crate) fn impl_module_for(
        &self,
        receiver: ImplReceiver<'_>,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        let ast = self.trait_impl_modules.get(receiver, trait_name);
        let syn = self
            .synthesised
            .as_ref()
            .and_then(|s| s.trait_impl_modules.get(receiver, trait_name));
        pick_module_union(ast, syn, type_module)
    }

    /// Every impl entry whose target *head* matches `receiver`, across all
    /// declaring modules. The keyed lookups are exact; this is the widened
    /// form for callers that cannot canonicalise — monomorphize and synthesis
    /// run after elaboration and hold a type's name without its import
    /// context. Prefer a keyed lookup wherever a module is known.
    pub(crate) fn entries_by_receiver<'a>(
        &'a self,
        receiver: &'a name::Receiver,
    ) -> impl Iterator<Item = &'a (ModuleSource, AstId)> + 'a {
        self.by_receiver
            .get(receiver)
            .into_iter()
            .flat_map(|entries| entries.iter())
    }

    /// Collected form of [`Self::entries_by_receiver`], for callers that need
    /// to iterate the widened match more than once.
    pub(crate) fn entries_by_receiver_vec(
        &self,
        receiver: &name::Receiver,
    ) -> Vec<(ModuleSource, AstId)> {
        self.entries_by_receiver(receiver).cloned().collect()
    }

    /// Receiver-matched form of [`Self::has_any_methodful_impl`].
    pub(crate) fn has_any_methodful_impl_by_receiver(
        &self,
        receiver: &name::Receiver,
        trait_name: &str,
        trait_ref: Option<crate::resolve::DeclRef>,
    ) -> bool {
        self.entries_by_receiver(receiver)
            .any(|entry| self.methodful_header_matches(entry, trait_name, trait_ref))
    }

    /// Receiver-matched form of [`Self::has_methodful_impl`].
    pub(crate) fn has_methodful_impl_by_receiver(
        &self,
        receiver: &name::Receiver,
        trait_name: &str,
        module_source: &ModuleSource,
    ) -> bool {
        self.entries_by_receiver(receiver).any(|entry| {
            entry.0 == *module_source && self.methodful_header_matches(entry, trait_name, None)
        })
    }

    /// Whether an inherent `impl` on `receiver` declares `method_name`.
    pub(crate) fn has_inherent_method_by_receiver(
        &self,
        receiver: &name::Receiver,
        method_name: &str,
    ) -> bool {
        self.all_by_receiver
            .get(receiver)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .any(|key| {
                self.impl_headers.get(key).is_some_and(|h| {
                    h.trait_name.is_none() && h.methods.iter().any(|m| m.name == method_name)
                })
            })
    }

    fn methodful_header_matches(
        &self,
        entry: &(ModuleSource, AstId),
        trait_name: &str,
        trait_ref: Option<crate::resolve::DeclRef>,
    ) -> bool {
        self.impl_headers.get(entry).is_some_and(|header| {
            if header.methods.is_empty() {
                return false;
            }
            match (trait_ref, header.trait_ref) {
                // Both sides named a declaration, so this is a question about
                // declarations. Only `Decl` qualifies: `Unresolved` is a unit
                // variant, so comparing raw answers would make any two
                // undeclared traits the same one.
                (
                    Some(crate::resolve::DeclRef::Decl(query)),
                    Some(crate::resolve::DeclRef::Decl(decl)),
                ) => query == decl,
                // A caller not yet carrying an identity falls back to the
                // spelling, which two modules can share (WEP 2026-08-10 stage
                // C has these still to convert).
                _ => header.trait_name.as_deref() == Some(trait_name),
            }
        })
    }

    /// Return the home module of a *value* blanket (`impl<T: Bound> Trait for
    /// T`) for `trait_name`, if one exists — `value_blanket_for_trait` excludes
    /// ref blankets, so a `impl<T: Inspect> Inspect for &T` is never returned for
    /// a value receiver. `type_module` is preferred as a stable tie-breaker when
    /// several modules host a value blanket for the trait.
    pub(crate) fn blanket_impl_module_for_trait(
        &self,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        self.value_blanket_for_trait(trait_name, type_module)
            .map(|b| &b.module)
    }

    /// The value blanket for `trait_name` whose receiver-param bounds `satisfies`
    /// accepts. A trait may carry several disjoint value blankets — the four
    /// reflection kinds each derive `Inspect` over their own `Reflect*` bound —
    /// so a receiver-blind first-wins selection would hand every receiver the
    /// first-registered kind and then reject it on the bound check.
    pub(crate) fn value_blanket_for_receiver(
        &self,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
        satisfies: &dyn Fn(&[BlanketBound]) -> bool,
    ) -> Option<&BlanketImpl> {
        let impls = self.blanket_impls.get(trait_name)?;
        let mut values = impls
            .iter()
            .filter(|b| b.receiver == BlanketReceiver::Value)
            .filter(|b| satisfies(&b.bounds));
        if let Some(hint) = type_module
            && let Some(b) = values.clone().find(|b| &b.module == hint)
        {
            return Some(b);
        }
        values.next()
    }

    /// The value blanket `impl<Param: Bounds, ..> Trait for Param` for
    /// `trait_name`, preferring one homed in `type_module`, else the first
    /// registered. Ref blankets (`impl<T> Trait for &T`) are excluded — they
    /// never dispatch a value receiver.
    fn value_blanket_for_trait(
        &self,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
    ) -> Option<&BlanketImpl> {
        let impls = self.blanket_impls.get(trait_name)?;
        let mut values = impls
            .iter()
            .filter(|b| b.receiver == BlanketReceiver::Value);
        if let Some(hint) = type_module
            && let Some(b) = values.clone().find(|b| &b.module == hint)
        {
            return Some(b);
        }
        values.next()
    }

    /// Whether `trait_name` has a *universal* ref blanket
    /// `impl<T: Bound> Trait for &T` (`is_mut` selects `&mut T`) — the inner is a
    /// bare type param, so it applies to every reference. Distinguished from a
    /// shape ref impl (`impl<T> IntoIterator for &List<T>`), whose inner is a
    /// concrete/parametric type. Callers route a `&<pointee>` type-param dispatch
    /// through the universal blanket only when one exists.
    pub(crate) fn has_universal_ref_blanket(&self, trait_name: &str, is_mut: bool) -> bool {
        self.blanket_impls.get(trait_name).is_some_and(|impls| {
            impls
                .iter()
                .any(|b| b.receiver == BlanketReceiver::Ref { is_mut })
        })
    }

    /// What determines each of a blanket impl's parameters, in declaration
    /// order — see [`blanket_param_sources`].
    pub(crate) fn blanket_param_sources(&self, blanket: &BlanketImpl) -> Vec<BlanketParamSource> {
        self.blanket_param_sources
            .get(&(blanket.module.clone(), blanket.ast_id))
            .cloned()
            .unwrap_or_default()
    }

    /// The `(declaring trait, associated type)` pairs a value blanket projects
    /// into type packs, in the order the impl declares them —
    /// `impl<T: Bound<Assoc = [..P]>, ..P> Trait for T` yields
    /// one pair, keyed by `Bound`'s declaration. The trait is carried because a bare assoc name
    /// is ambiguous: the reflection kinds all spell their member channel
    /// `Members`.
    pub(crate) fn pack_assocs_of_blanket(&self, blanket: &BlanketImpl) -> Vec<(DeclKey, String)> {
        self.blanket_pack_assocs
            .get(&(blanket.module.clone(), blanket.ast_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Like [`impl_module_for`] but only returns a hit when the impl block
    /// is **fully concrete** (no `impl<T, …>` type parameters). Used by
    /// the monomorphizer when redirecting a substituted trait-method call
    /// to the impl that actually defines its body: a concrete impl's
    /// function lives in the impl block's module, while a generic impl's
    /// post-substitution instance is materialised in the receiver type's
    /// module by convention. Mirrors the legacy `trait_method_locations`
    /// semantics that filtered on `impl_type_params.is_empty()`.
    pub(crate) fn concrete_impl_module_for(
        &self,
        receiver: ImplReceiver<'_>,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        let ast = self.concrete_trait_impl_modules.get(receiver, trait_name);
        let syn = self
            .synthesised
            .as_ref()
            .and_then(|s| s.concrete_trait_impl_modules.get(receiver, trait_name));
        pick_module_union(ast, syn, type_module)
    }

    /// The stdlib's declaration of `name`, as the key an `impl` header
    /// resolves to. A compiler item (`Member`, `ReflectStruct`, …) is a
    /// stdlib trait, so this is its identity — and a user trait sharing the
    /// name is a different declaration, not an exemption to special-case.
    pub(super) fn stdlib_trait_decl_key(&self, name: &str) -> Option<ImplTargetKey> {
        self.decl_index
            .keys()
            .find(|(ms, n)| n == name && !is_user_local(ms))
            .map(|key| ImplTargetKey::Decl(key.clone()))
    }

    /// The digested declaration an `impl` header's [`ImplHeader::trait_key`]
    /// names, or `None` when the key names no trait declaration.
    pub(super) fn trait_decl_header(&self, key: &ImplTargetKey) -> Option<&TraitDeclHeader> {
        let ImplTargetKey::Decl(decl_key) = key else {
            return None;
        };
        let loc = self.decl_index.get(decl_key)?;
        self.trait_decl_headers.get(loc)
    }

    /// Produce a new `TraitEnv` with the synthesis-layer impls populated.
    ///
    /// `synth_impls` lists every `(type_name, trait_name) -> ModuleSource`
    /// triple discovered in TIR after the synthesis phase has finished
    /// adding auto-derived / generated impls. Designed to be called once
    /// per pipeline run (the synthesis phase) — calling again replaces the
    /// existing layer.
    ///
    /// `prev` must be the unique owner of the inner `TraitEnv`
    /// (`Arc::strong_count == 1`). Since `TraitEnv: !Clone`, this is the
    /// only viable extension shape: we move out of the `Arc`, swap one
    /// field, and re-wrap. Callers are responsible for not handing this
    /// function a shared `Arc`.
    pub fn extend_with_synthesised(prev: Arc<Self>, synth_impls: SynthesisedImpls) -> Arc<Self> {
        let Ok(mut env) = Arc::try_unwrap(prev) else {
            panic!("extend_with_synthesised: TraitEnv Arc must be uniquely owned")
        };
        env.synthesised = Some(synth_impls);
        Arc::new(env)
    }
}

/// Which namespace an impl-module query spells its receiver in.
///
/// The index answers in two, and they are not interchangeable: a mangled fq
/// receiver picks out one declaration, a declared name picks out any
/// declaration spelling itself that way. Each namespace has its own storage,
/// written from one receiver identity, so a query cannot land in the wrong one
/// — see WEP 2026-08-10.
///
/// [`Self::Of`] carries the identity and lets the index derive both spellings;
/// the other two are for callers that hold only one. A bare `&str` cannot claim
/// to be mangled.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ImplReceiver<'a> {
    /// The receiver itself. Both spellings are derived from it here, so the
    /// query names no namespace and cannot name the wrong one.
    Of(&'a name::Receiver),
    /// A receiver with its type arguments applied (`List<…/Token>`). Only the
    /// mangled namespace can spell an instantiation.
    Instantiated(&'a name::MangledName),
    /// A declaration name and nothing more. Carries no module, so it cannot
    /// separate two modules' same-named types — which is why it is a distinct
    /// variant rather than a receiver a caller flattened.
    Declared(&'a name::DeclName),
}

impl ImplReceiver<'_> {
    /// The spelling this query names its receiver by, for the callers that key
    /// their own in-pass state on the same string.
    pub(crate) fn spelling(self) -> String {
        match self {
            ImplReceiver::Of(r) => r.head_key().into_string(),
            ImplReceiver::Instantiated(m) => m.as_mangled_str().to_string(),
            ImplReceiver::Declared(d) => d.as_decl_str().to_string(),
        }
    }
}

/// A receiver a lookup may try, kept in the form the thing that produced it
/// had. A candidate list is assembled from several sources — a method info's
/// receiver, a mangled struct key — and they are not one namespace. Carrying
/// each in its own form is what keeps the query from having to guess.
#[derive(Debug, Clone)]
pub(crate) enum ReceiverCandidate {
    Of(name::Receiver),
    Instantiated(name::MangledName),
    Declared(name::DeclName),
}

impl ReceiverCandidate {
    pub(crate) fn as_receiver(&self) -> ImplReceiver<'_> {
        match self {
            ReceiverCandidate::Of(r) => ImplReceiver::Of(r),
            ReceiverCandidate::Instantiated(m) => ImplReceiver::Instantiated(m),
            ReceiverCandidate::Declared(d) => ImplReceiver::Declared(d),
        }
    }
}

/// The static-method index's bucket for an impl whose target keys as
/// `type_key`.
///
/// A declared target is that declaration, so the two indexes agree by
/// construction rather than by two derivations happening to match. A blanket
/// parameter or a reference kind names no declaration, so those still ask.
fn static_receiver_key(type_key: &ImplTargetKey, otherwise: impl FnOnce() -> DeclKey) -> DeclKey {
    match type_key {
        ImplTargetKey::Decl(key) => key.clone(),
        ImplTargetKey::Ref(_) | ImplTargetKey::TypeParam(..) | ImplTargetKey::Builtin(_) => {
            otherwise()
        }
    }
}

/// The impl header's target, from the site the header wrote.
///
/// A site behind no declaration — a tuple, a function type, a name that
/// reaches nothing — is keyed to the impl's own module. Nothing else claims
/// it, and coherence for exactly those is decided per module.
fn impl_target_key_at(
    ty: &ast::Type,
    module_source: &ModuleSource,
    symbols: &SymbolTable,
    resolutions: &crate::resolve::Resolutions,
) -> ImplTargetKey {
    sited_impl_target_key(ty, module_source, resolutions).unwrap_or_else(|| {
        ImplTargetKey::of_written(
            &get_type_name_static(ty),
            module_source,
            symbols,
            resolutions,
        )
    })
}

/// The one trait declaration named `name`, else the one effect or resource —
/// the order and the per-family uniqueness of
/// [`TraitEnv::unique_trait_decl_key`] and
/// [`TraitEnv::unique_effect_or_resource_decl_key`], which is what
/// `Elaborator::decl_key_or_local` runs. Asking the three families at once
/// instead would decline where the elaborator answers: a `trait Encode` beside
/// another module's `interface Encode` is one trait, not an ambiguity.
///
/// Declines when a family holds two, since guessing between two same-named
/// declarations is the mis-identification this design prevents.
///
/// One divergence remains: `decl_key_or_local` consults its struct-like index
/// *before* the trait one, so a name declared as a struct in one module and a
/// trait in another keys to the trait here and to the struct there. This is a
/// trait position and the struct is the wrong answer for it, but the two
/// should agree — the fix is to give the elaborator a trait-position entry
/// point rather than to copy the struct-first order into a trait lookup.
fn unique_declared_trait<L, E, R>(
    name: &str,
    decls: &IndexMap<DeclKey, L>,
    effects: &IndexMap<DeclKey, E>,
    resources: &IndexMap<DeclKey, R>,
) -> Option<DeclKey> {
    let unique = |mut hits: Box<dyn Iterator<Item = &DeclKey> + '_>| {
        let first = hits.next()?;
        hits.next().is_none().then(|| first.clone())
    };
    unique(Box::new(decls.keys().filter(|(_, n)| n == name))).or_else(|| {
        unique(Box::new(
            effects
                .keys()
                .chain(resources.keys())
                .filter(|(_, n)| n == name),
        ))
    })
}

/// The key an `impl` header's target resolves to, from the site the header
/// wrote — the vantage the target name belongs to.
///
/// `None` where the site names no declaration: a builtin shape, a name the walk
/// could not resolve, or a position with no head at all. Those keep
/// [`impl_target_key_at`], whose fallback keys them to the impl's own module.
fn sited_impl_target_key(
    ty: &ast::Type,
    module_source: &ModuleSource,
    resolutions: &crate::resolve::Resolutions,
) -> Option<ImplTargetKey> {
    // A reference target buckets by kind alone: the table resolves `&List<T>`
    // to `List`, which is the referent, not the bucket.
    if let Some(kind) = name::RefKind::from_ast(ty) {
        return Some(ImplTargetKey::Ref(kind));
    }
    let site = crate::resolve::head_site(ty)?;
    match resolutions.get(site)? {
        // The impl's own binder, which shadows any declaration of that name —
        // `impl<T> Trait for T` written where a `struct T` exists stays a
        // blanket.
        crate::resolve::DeclRef::Binder(_) => Some(ImplTargetKey::TypeParam(
            module_source.clone(),
            get_type_name_static(ty),
        )),
        answer @ crate::resolve::DeclRef::Decl(_) => resolutions
            .decl_named(answer)
            .map(|(module, name)| ImplTargetKey::of_decl(module, name)),
        crate::resolve::DeclRef::Unresolved => None,
    }
}

/// Returns `true` if the module source is a user-local module (part of the current package).
pub(super) fn is_user_local(ms: &ModuleSource) -> bool {
    matches!(
        ms,
        ModuleSource::Local { .. }
            | ModuleSource::Dependency { .. }
            | ModuleSource::EntryPoint { .. }
            | ModuleSource::Redirected { .. }
    )
}

/// The declarations a user package owns, as identities rather than bare
/// names. The orphan rule asks "does this package own the thing this name
/// refers to?" — a question a spelling cannot answer, because a user
/// declaration shadowing a stdlib name would otherwise vouch for the stdlib
/// type it shadows.
struct LocalDecls {
    types: IndexSet<DeclKey>,
    traits: IndexSet<DeclKey>,
    /// Whether a user module declares the tuple type (`pub type [..T];`). The
    /// tuple is one global shape rather than a per-module declaration, so
    /// ownership of it is a yes/no fact about the package.
    tuple: bool,
}

/// Describes the orphan-rule "classification" of a position in the impl sequence.
enum PositionKind {
    /// The outermost type constructor is a user-local type.
    LocalType,
    /// The position is a bare uncovered type parameter.
    UncoveredTypeParam,
    /// The outermost type constructor is a foreign (non-local) type.
    ForeignType,
}

/// Classify the outermost type constructor of an AST type relative to the orphan rule.
///
/// RFC 2451 sequence rule: walk `[self_type, trait_arg1, ...]` left-to-right.
/// - `LocalType` at position i, with no `UncoveredTypeParam` seen before i → **allowed**.
/// - `UncoveredTypeParam` before any `LocalType` → **forbidden**.
///
/// References (`&T`, `&mut T`) are *fundamental* and are looked through.
fn classify_position(
    ty: &Type,
    header: &ImplHeader,
    local: &LocalDecls,
    resolve: ResolveWritten<'_>,
) -> PositionKind {
    match ty {
        // Fundamental: look through references
        Type::Reference(inner) | Type::MutReference(inner) => {
            classify_position(inner, header, local, resolve)
        }
        // "Is this position an uncovered parameter?" is a question about the
        // impl's own binders, answered without resolving anything — and it must
        // be asked separately. `ImplTargetKey::TypeParam` covers both a binder
        // and a name that reaches no declaration at all, and reading the second
        // as uncovered loses the coherence error an `impl Undeclared { … }`
        // deserves while inventing an orphan violation for
        // `impl From<Local> for Undeclared`.
        Type::Named(_) | Type::Generic(_)
            if super::written::binder_of(ty, &header.type_params).is_some() =>
        {
            PositionKind::UncoveredTypeParam
        }
        // Everything else is an identity question: the package owns this
        // position only when the name resolves to a declaration it owns. A name
        // resolving to nothing is foreign, not uncovered.
        Type::Named(_) | Type::Generic(_) | Type::NamespacedGeneric(_) => {
            match resolve(&header.module, ty, &header.type_params) {
                ImplTargetKey::Decl(key) if local.types.contains(&key) => PositionKind::LocalType,
                ImplTargetKey::Decl(_)
                | ImplTargetKey::Ref(_)
                | ImplTargetKey::TypeParam(..)
                | ImplTargetKey::Builtin(_) => PositionKind::ForeignType,
            }
        }
        // Tuples are local if the current crate owns them (via `pub type [..T];`)
        Type::Tuple(_) if local.tuple => PositionKind::LocalType,
        Type::Tuple(_)
        | Type::Function(_)
        | Type::TypePackSpread(..)
        | Type::Infer(_)
        | Type::Error(_) => PositionKind::ForeignType,
    }
}

/// Check the RFC 2451 orphan rule for a single impl block that has a foreign trait.
///
/// Sequence: `[self_type, trait_arg1, trait_arg2, ...]`.
/// Valid if there exists a position with `LocalType` and no `UncoveredTypeParam` before it.
fn check_orphan_rfc2451(
    header: &ImplHeader,
    local: &LocalDecls,
    resolve: ResolveWritten<'_>,
) -> bool {
    // Build the sequence: self type first, then trait type arguments
    let trait_args: &[Type] = match header.trait_type.as_ref() {
        Some(Type::Generic(g)) => &g.args,
        _ => &[],
    };

    let mut seen_uncovered_before_local = false;

    // Position 0: self type
    match classify_position(&header.ty, header, local, resolve) {
        PositionKind::LocalType => return true,
        PositionKind::UncoveredTypeParam => seen_uncovered_before_local = true,
        PositionKind::ForeignType => {}
    }

    // Positions 1+: trait type arguments
    for trait_arg in trait_args {
        match classify_position(trait_arg, header, local, resolve) {
            PositionKind::LocalType => {
                if !seen_uncovered_before_local {
                    return true;
                }
                // Uncovered param was seen before this local type → still violated
                return false;
            }
            PositionKind::UncoveredTypeParam => {
                seen_uncovered_before_local = true;
            }
            PositionKind::ForeignType => {}
        }
    }

    false
}

/// Resolves a supertrait name referenced in a trait's own module to that
/// supertrait's declaration. `None` for a name that declares no trait.
type ResolveTrait<'a> = &'a dyn Fn(&ModuleSource, &ast::TraitBound) -> Option<TraitDeclLoc>;

/// Resolves a type written in one module — the vantage — to the declaration it
/// names, shadowed by the surrounding item's own type parameters. The single
/// answer to "which type is this name?", handed to whole-program checks so
/// none of them re-derives one from a bare head.
type ResolveWritten<'a> =
    &'a dyn Fn(&ModuleSource, &ast::Type, &[ast::GenericParam]) -> ImplTargetKey;

/// Add an inherited bound unless the list already holds its declaration, so
/// two spellings of one supertrait collapse.
fn push_unique_inherited(bounds: &mut Vec<InheritedBound>, bound: &InheritedBound) {
    let Some(existing) = bounds.iter_mut().find(|b| b.decl == bound.decl) else {
        bounds.push(bound.clone());
        return;
    };
    if existing.bound.assoc_types.is_empty() && !bound.bound.assoc_types.is_empty() {
        *existing = bound.clone();
    }
}

/// Expand every trait's direct supertraits into its transitive closure,
/// reporting each trait that reaches itself. A cycle's edge is cut rather than
/// followed, keeping the closure finite.
fn build_supertrait_closures(
    headers: &IndexMap<TraitDeclLoc, TraitDeclHeader>,
    resolve: ResolveTrait<'_>,
) -> (SupertraitClosureIndex, Vec<(ModuleSource, TypeError)>) {
    let mut closures = SupertraitClosureIndex::default();
    if headers.values().all(|h| h.supertraits.is_empty()) {
        return (closures, Vec::new());
    }
    let mut cycles = Vec::new();
    let mut reported: IndexSet<TraitDeclLoc> = IndexSet::default();
    for loc in headers.keys() {
        let mut stack = Vec::new();
        expand_supertraits(
            loc,
            headers,
            resolve,
            &mut closures,
            &mut stack,
            &mut reported,
            &mut cycles,
        );
    }
    (closures, cycles)
}

fn expand_supertraits(
    loc: &TraitDeclLoc,
    headers: &IndexMap<TraitDeclLoc, TraitDeclHeader>,
    resolve: ResolveTrait<'_>,
    closures: &mut SupertraitClosureIndex,
    stack: &mut Vec<TraitDeclLoc>,
    reported: &mut IndexSet<TraitDeclLoc>,
    cycles: &mut Vec<(ModuleSource, TypeError)>,
) -> Vec<InheritedBound> {
    if let Some(done) = closures.get(loc) {
        return done.clone();
    }
    let Some(header) = headers.get(loc) else {
        return Vec::new();
    };

    stack.push(loc.clone());
    let mut closure: Vec<InheritedBound> = Vec::new();
    for direct in &header.supertraits {
        let Some(super_loc) = resolve(&loc.0, direct) else {
            // Blame the declaration, not every implementor of it.
            cycles.push((
                loc.0.clone(),
                TypeError::UnknownSupertrait {
                    trait_name: header.name.clone(),
                    supertrait: direct.name.clone(),
                    span: direct.span,
                },
            ));
            continue;
        };
        // Before the push: `trait Loop: Loop` must not land in its own closure.
        if let Some(pos) = stack.iter().position(|s| *s == super_loc) {
            report_supertrait_cycle(pos, stack, headers, reported, cycles);
            continue;
        }
        // `direct.name` is meaningful only here; record what `resolve` named.
        let super_decl = headers
            .get(&super_loc)
            .expect("resolve answers with a header's own location")
            .name
            .clone();
        push_unique_inherited(
            &mut closure,
            &InheritedBound {
                bound: direct.clone(),
                decl: (super_loc.0.clone(), super_decl),
            },
        );
        for inherited in expand_supertraits(
            &super_loc, headers, resolve, closures, stack, reported, cycles,
        ) {
            push_unique_inherited(&mut closure, &inherited);
        }
    }
    stack.pop();

    closures.insert(loc.clone(), closure.clone());
    closure
}

/// Re-key the closures by bare trait name, dropping any name more than one
/// module declares — an ambiguous name must not silently pick a closure.
fn index_closures_by_name(
    headers: &IndexMap<TraitDeclLoc, TraitDeclHeader>,
    closures: &SupertraitClosureIndex,
) -> IndexMap<String, Vec<InheritedBound>> {
    let mut by_name: IndexMap<String, Option<Vec<InheritedBound>>> = IndexMap::default();
    for (loc, header) in headers {
        let closure = closures.get(loc).cloned().unwrap_or_default();
        by_name
            .entry(header.name.clone())
            .and_modify(|slot| *slot = None)
            .or_insert(Some(closure));
    }
    by_name
        .into_iter()
        .filter_map(|(name, closure)| closure.map(|c| (name, c)))
        .collect()
}

/// Report the cycle closed by the edge back to `stack[pos]`, attributing it to
/// that trait — the one that turned out to be its own supertrait.
fn report_supertrait_cycle(
    pos: usize,
    stack: &[TraitDeclLoc],
    headers: &IndexMap<TraitDeclLoc, TraitDeclHeader>,
    reported: &mut IndexSet<TraitDeclLoc>,
    cycles: &mut Vec<(ModuleSource, TypeError)>,
) {
    let culprit = &stack[pos];
    if !reported.insert(culprit.clone()) {
        return;
    }
    let Some(header) = headers.get(culprit) else {
        return;
    };
    let mut chain: Vec<String> = stack[pos..]
        .iter()
        .filter_map(|s| headers.get(s).map(|h| h.name.clone()))
        .collect();
    chain.push(header.name.clone());
    cycles.push((
        culprit.0.clone(),
        TypeError::CircularSupertrait {
            trait_name: header.name.clone(),
            chain,
            span: header.span,
        },
    ));
}

enum VariadicTarget {
    /// The bare `[..T]`, the only shape the compiler implements.
    PackOnly,
    /// A pack beside other elements (`[i32, ..T]`) or under a reference.
    Unsupported,
}

/// Classify an impl target that spreads a type pack; `None` when it spreads
/// none. Only a tuple can carry one.
fn variadic_target(ty: &ast::Type) -> Option<VariadicTarget> {
    match ty {
        ast::Type::Tuple(elems) => {
            if !elems
                .iter()
                .any(|e| matches!(e, ast::Type::TypePackSpread(..)))
            {
                return None;
            }
            Some(if elems.len() == 1 {
                VariadicTarget::PackOnly
            } else {
                VariadicTarget::Unsupported
            })
        }
        // A pack under a reference never reaches the impl's type-param scope,
        // so type resolution would report the declared pack as unknown.
        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
            variadic_target(inner).map(|_| VariadicTarget::Unsupported)
        }
        _ => None,
    }
}

/// Whether two impl-written types can denote the same type. An impl's own type
/// parameter is a wildcard. An undecidable pair unifies: for a coherence rule,
/// reporting is the sound direction.
fn types_can_unify(
    a: &ast::Type,
    a_params: &IndexSet<&str>,
    b: &ast::Type,
    b_params: &IndexSet<&str>,
) -> bool {
    let is_wildcard = |ty: &ast::Type, params: &IndexSet<&str>| match ty {
        ast::Type::Named(named) => params.contains(named.name.as_str()),
        _ => false,
    };
    if is_wildcard(a, a_params) || is_wildcard(b, b_params) {
        return true;
    }
    let unify_all = |xs: &[ast::Type], ys: &[ast::Type]| {
        xs.len() == ys.len()
            && xs
                .iter()
                .zip(ys)
                .all(|(x, y)| types_can_unify(x, a_params, y, b_params))
    };
    match (a, b) {
        (ast::Type::Named(x), ast::Type::Named(y)) => x.name == y.name,
        (ast::Type::Generic(x), ast::Type::Generic(y)) => {
            x.name == y.name && unify_all(&x.args, &y.args)
        }
        (ast::Type::Tuple(xs), ast::Type::Tuple(ys)) => unify_all(xs, ys),
        (ast::Type::Reference(x), ast::Type::Reference(y))
        | (ast::Type::MutReference(x), ast::Type::MutReference(y)) => {
            types_can_unify(x, a_params, y, b_params)
        }
        // Decidable shapes that did not pair up above have different heads.
        (
            ast::Type::Named(_)
            | ast::Type::Generic(_)
            | ast::Type::Tuple(_)
            | ast::Type::Reference(_)
            | ast::Type::MutReference(_),
            ast::Type::Named(_)
            | ast::Type::Generic(_)
            | ast::Type::Tuple(_)
            | ast::Type::Reference(_)
            | ast::Type::MutReference(_),
        ) => false,
        // Projections, function types, nested packs and placeholders are not
        // decidable here.
        (
            ast::Type::NamespacedGeneric(_)
            | ast::Type::Function(_)
            | ast::Type::TypePackSpread(..)
            | ast::Type::Infer(_)
            | ast::Type::Error(_),
            _,
        )
        | (
            _,
            ast::Type::NamespacedGeneric(_)
            | ast::Type::Function(_)
            | ast::Type::TypePackSpread(..)
            | ast::Type::Infer(_)
            | ast::Type::Error(_),
        ) => true,
    }
}

struct VariadicImpl<'a> {
    module_source: &'a ModuleSource,
    span: Span,
    trait_name: String,
    trait_args: &'a [ast::Type],
    params: IndexSet<&'a str>,
}

impl VariadicImpl<'_> {
    /// Whether the two accept a common tuple. Both targets are the bare
    /// `[..T]`, so only the trait's own arguments can hold them apart:
    /// `Conv<i32>` and `Conv<String>` implement different things.
    fn overlaps(&self, other: &Self) -> bool {
        self.trait_args.len() == other.trait_args.len()
            && self
                .trait_args
                .iter()
                .zip(other.trait_args)
                .all(|(a, b)| types_can_unify(a, &self.params, b, &other.params))
    }
}

/// Coherence Rule 2 (WEP 2026-03-14 §5): two variadic impls of one trait accept
/// the same tuples, and a pack's bounds are resolved only at monomorphization,
/// so nothing separates them at selection. Reject the later one where it is
/// written; a stdlib impl is considered but never reported, being unfixable by
/// the user.
///
/// Grouping is by trait *declaration*, so two modules may each declare a `Tag`
/// and keep their own variadic impls.
///
/// The same walk refuses a target the compiler does not implement, which would
/// otherwise miscompile or trip the WIR validator.
fn check_variadic_impl_overlap(
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
) -> Vec<(ModuleSource, TypeError)> {
    let mut violations = Vec::new();
    let mut groups: IndexMap<&ImplTargetKey, Vec<VariadicImpl<'_>>> = IndexMap::default();

    for header in impl_headers.values() {
        let Some(trait_key) = &header.trait_key else {
            continue;
        };
        let Some(target) = variadic_target(&header.ty) else {
            continue;
        };
        if let VariadicTarget::Unsupported = target {
            if is_user_local(&header.module) {
                violations.push((
                    header.module.clone(),
                    TypeError::UnsupportedVariadicImplTarget { span: header.span },
                ));
            }
            continue;
        }
        let ImplTargetKey::Decl(_) = trait_key else {
            continue;
        };
        groups.entry(trait_key).or_default().push(VariadicImpl {
            module_source: &header.module,
            span: header.span,
            trait_name: trait_key.display_name().to_string(),
            trait_args: match header.trait_type.as_ref() {
                Some(ast::Type::Generic(generic)) => &generic.args,
                _ => &[],
            },
            params: header.type_params.iter().map(|p| p.name.as_str()).collect(),
        });
    }

    for impls in groups.values_mut() {
        // A stdlib impl holds its ground; among user impls the earlier one in
        // (file, position) order does. The module map's order is load order,
        // which is neither source order nor stable across entry points.
        impls.sort_by_key(|i| {
            (
                is_user_local(i.module_source),
                i.module_source.to_string(),
                i.span.start,
            )
        });
        let mut held: Vec<&VariadicImpl<'_>> = Vec::new();
        for candidate in impls.iter() {
            let Some(conflict) = held.iter().find(|h| h.overlaps(candidate)) else {
                held.push(candidate);
                continue;
            };
            if !is_user_local(candidate.module_source) {
                continue;
            }
            violations.push((
                candidate.module_source.clone(),
                TypeError::OverlappingVariadicImpls {
                    trait_name: candidate.trait_name.clone(),
                    self_type_name: "[..]".to_string(),
                    conflicting_impl: if conflict.module_source == candidate.module_source {
                        "the earlier one in this file".to_string()
                    } else {
                        format!("the one in `{}`", conflict.module_source)
                    },
                    span: candidate.span,
                },
            ));
        }
    }

    violations
}

/// Whether the target names one of the impl's own type parameters, making the
/// impl generic over the head rather than written for one instantiation.
fn target_mentions_impl_param(ty: &ast::Type, params: &IndexSet<&str>) -> bool {
    match ty {
        ast::Type::Named(named) => params.contains(named.name.as_str()),
        ast::Type::Generic(generic) => generic
            .args
            .iter()
            .any(|a| target_mentions_impl_param(a, params)),
        ast::Type::Tuple(elems) => elems.iter().any(|e| target_mentions_impl_param(e, params)),
        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
            target_mentions_impl_param(inner, params)
        }
        ast::Type::TypePackSpread(name, _) => params.contains(name.as_str()),
        ast::Type::NamespacedGeneric(_)
        | ast::Type::Function(_)
        | ast::Type::Infer(_)
        | ast::Type::Error(_) => false,
    }
}

/// An inherent `impl Box_<i32>` and an inherent `impl<T> Box_<T>` defining the
/// same method both own the name `Box_<i32>::a`. A trait impl would force one
/// signature on both, letting coherence Rule 1 pick the specific one; an
/// inherent impl carries no such contract, so a generic caller type-checked
/// against the general method would link to a differently-typed function.
/// Rejected, as in Rust.
///
/// Keyed by the target's resolved [`ImplTargetKey`], never by the head as
/// written: `Box_` in one module and `Box_` in another are two types, and a
/// spelling cannot tell them apart. Reading the resolved key off
/// [`ImplHeader`] is what makes that structural — the identity is decided once,
/// where the vantage exists, rather than re-derived here from a bare name.
fn check_inherent_impl_collisions(
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
) -> Vec<(ModuleSource, TypeError)> {
    let mut generic_methods_by_target: IndexMap<&ImplTargetKey, IndexSet<&str>> =
        IndexMap::default();
    let mut instantiations = Vec::new();

    for header in impl_headers.values() {
        if header.trait_key.is_some() {
            continue;
        }
        let params: IndexSet<&str> = header.type_params.iter().map(|p| p.name.as_str()).collect();
        if target_mentions_impl_param(&header.ty, &params) {
            generic_methods_by_target
                .entry(&header.target)
                .or_default()
                .extend(header.methods.iter().map(|m| m.name.as_str()));
        } else if is_user_local(&header.module) {
            instantiations.push(header);
        }
    }

    let mut violations = Vec::new();
    for header in instantiations {
        let Some(generic_methods) = generic_methods_by_target.get(&header.target) else {
            continue;
        };
        for method in &header.methods {
            if generic_methods.contains(method.name.as_str()) {
                violations.push((
                    header.module.clone(),
                    TypeError::DuplicateInherentMethod {
                        self_type_name: header.target.display_name().to_string(),
                        method_name: method.name.clone(),
                        span: method.span,
                    },
                ));
            }
        }
    }

    violations
}

/// Check orphan rules for all trait impl blocks across all modules.
/// Only impl blocks in local (user) modules are checked. Each violation is
/// paired with the offending impl's [`ModuleSource`] for file attribution.
fn check_all_orphan_rules(
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    decl_index: &TraitDeclIndex,
    type_decl_index: &IndexMap<DeclKey, ModuleSource>,
    resolve: ResolveWritten<'_>,
) -> Vec<(ModuleSource, TypeError)> {
    let mut violations = Vec::new();

    let local_key = |(_, ms): (&DeclKey, &ModuleSource)| is_user_local(ms);
    let local = LocalDecls {
        types: type_decl_index
            .iter()
            .filter(|e| local_key((e.0, e.1)))
            .map(|(key, _)| key.clone())
            .collect(),
        traits: decl_index
            .iter()
            .filter(|(_, (ms, _))| is_user_local(ms))
            .map(|(key, _)| key.clone())
            .collect(),
        tuple: type_decl_index
            .iter()
            .any(|((_, name), ms)| name == TypeTable::TUPLE_TYPE_NAME && is_user_local(ms)),
    };

    for header in impl_headers.values() {
        if !is_user_local(&header.module) {
            continue;
        }

        let Some(trait_key) = &header.trait_key else {
            // Inherent impl. The orphan rule (foreign-trait/foreign-type)
            // does not apply, but coherence does: a user package may only
            // define inherent methods on types it owns. Extending a foreign
            // type (a primitive, `Array<T>`, `String`, or any other stdlib
            // type) inherently would let two packages add colliding methods
            // to the same type, so it is forbidden — use a trait instead.
            // `classify_position` looks through references and treats a
            // `LocalType` head as owned; only a genuinely foreign head is a
            // violation. (Stdlib modules are skipped above, so their own
            // `impl Array<T>` / `impl i32` are unaffected.)
            if let PositionKind::ForeignType =
                classify_position(&header.ty, header, &local, resolve)
            {
                violations.push((
                    header.module.clone(),
                    TypeError::InherentImplOnForeignType {
                        self_type_name: header.target.display_name().to_string(),
                        span: header.span,
                    },
                ));
            }
            continue;
        };

        // If the trait is local, always allowed
        if matches!(trait_key, ImplTargetKey::Decl(key) if local.traits.contains(key)) {
            continue;
        }

        // Foreign trait: apply RFC 2451 sequence check
        if !check_orphan_rfc2451(header, &local, resolve) {
            violations.push((
                header.module.clone(),
                TypeError::OrphanViolation {
                    trait_name: trait_key.display_name().to_string(),
                    self_type_name: header.target.display_name().to_string(),
                    span: header.span,
                },
            ));
        }
    }

    violations
}

/// The declaration name an `impl` header writes its target as.
///
/// An AST header carries no module, so this cannot produce a qualified
/// receiver — compare it against [`name::Receiver::decl_key`], which is the
/// same namespace, never against `head_key`.
pub(super) fn receiver_decl_key(ty: &ast::Type) -> String {
    match name::RefKind::from_ast(ty) {
        Some(kind) => kind.prefix().to_string(),
        None => get_type_name_static(ty),
    }
}

/// [`get_type_name_static`] keeping the written type arguments
/// (`Stream<u8>`), for the spellings a mangled name embeds.
pub(super) fn get_type_name_full_static(ty: &ast::Type) -> String {
    match ty {
        ast::Type::Generic(generic) => {
            let args: Vec<String> = generic.args.iter().map(get_type_name_full_static).collect();
            // `, `, matching `Elaborator::get_type_name_full`: both render the
            // written trait type into one mangled segment, and a separator only
            // one of them uses splits a nested `Pair<i32, i32>` into two names.
            format!("{}<{}>", generic.name, args.join(", "))
        }
        ast::Type::Reference(inner) => format!("&{}", get_type_name_full_static(inner)),
        ast::Type::MutReference(inner) => format!("&mut {}", get_type_name_full_static(inner)),
        other => get_type_name_static(other),
    }
}

pub(super) fn get_type_name_static(ty: &ast::Type) -> String {
    match ty {
        ast::Type::Named(named) if named.name == "()" => TypeTable::UNIT_TYPE_NAME.to_string(),
        ast::Type::Named(named) => named.name.clone(),
        ast::Type::Generic(generic) => generic.name.clone(),
        ast::Type::Reference(_) | ast::Type::MutReference(_) => name::RefKind::from_ast(ty)
            .expect("Reference/MutReference classify")
            .prefix()
            .to_string(),
        ast::Type::Tuple(elems) => {
            if elems.is_empty() {
                TypeTable::UNIT_TYPE_NAME.to_string()
            } else {
                TypeTable::TUPLE_TYPE_NAME.to_string()
            }
        }
        // `geo::Tag` writes the declaration name `Tag`; the namespace says
        // which module declares it, which is a question for the reference
        // site, not for a spelling. Rendering these as `Unknown` filed them
        // under a name no lookup asks for and put `Unknown` in diagnostics.
        ast::Type::NamespacedGeneric(ns) => ns.name.clone(),
        _ => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{GenericParam, GenericType, NamedType};
    use crate::module_source::ModuleSourceInterner;
    use crate::token::Span;

    fn dummy_span() -> Span {
        Span {
            start: 0,
            end: 0,
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 1,
        }
    }

    fn named(name: &str) -> Type {
        Type::Named(NamedType {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            span: dummy_span(),
        })
    }

    fn generic(name: &str, args: Vec<Type>) -> Type {
        Type::Generic(GenericType {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            args,
            span: dummy_span(),
        })
    }

    fn ref_type(inner: Type) -> Type {
        Type::Reference(Box::new(inner))
    }

    fn mut_ref_type(inner: Type) -> Type {
        Type::MutReference(Box::new(inner))
    }

    fn type_param(name: &str) -> GenericParam {
        GenericParam {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            name_span: dummy_span(),
            is_effect: false,
            is_pack: false,
            bounds: vec![],
            default: None,
            span: dummy_span(),
        }
    }

    /// The vantage every orphan-rule test writes its impl from. One per
    /// thread: [`crate::intern::InternedStr`] compares by pointer, so a fresh
    /// interner per call would mint modules that never compare equal.
    fn vantage() -> ModuleSource {
        thread_local! {
            static VANTAGE: ModuleSource = ModuleSourceInterner::new().local("./test.wado");
        }
        VANTAGE.with(Clone::clone)
    }

    /// Names the test program declares nowhere. The real resolver answers
    /// `TypeParam` for these — the same variant a binder gets — so the double
    /// must too, or the tests cannot see the conflation `classify_position`
    /// must not fall for.
    const UNDECLARED: &[&str] = &["Undeclared"];

    /// Test double for the build-time resolver: a binder stays a binder, a
    /// reference keys by kind, an [`UNDECLARED`] name reaches no declaration,
    /// and every other name is declared by the module that wrote it.
    /// `make_local_decls` builds the owned set from the same module, so a name
    /// is local exactly when the test says it is.
    fn resolve_for_test(
        module: &ModuleSource,
        ty: &Type,
        type_params: &[GenericParam],
    ) -> ImplTargetKey {
        if let Some(kind) = name::RefKind::from_ast(ty) {
            return ImplTargetKey::Ref(kind);
        }
        let head = get_type_name_static(ty);
        if type_params.iter().any(|p| p.name == head) || UNDECLARED.contains(&head.as_str()) {
            return ImplTargetKey::TypeParam(module.clone(), head);
        }
        ImplTargetKey::Decl((module.clone(), head))
    }

    fn make_local_decls(local_names: &[&str]) -> LocalDecls {
        LocalDecls {
            types: local_names
                .iter()
                .map(|name| (vantage(), (*name).to_string()))
                .collect(),
            traits: IndexSet::default(),
            tuple: false,
        }
    }

    /// A digested header as `TraitEnv::build` would produce it, with the
    /// identities the test double resolves.
    fn impl_header(
        type_params: Vec<GenericParam>,
        trait_type: Type,
        self_type: Type,
    ) -> ImplHeader {
        let module = vantage();
        ImplHeader {
            target: resolve_for_test(&module, &self_type, &type_params),
            trait_key: Some(resolve_for_test(&module, &trait_type, &type_params)),
            // The orphan-rule tests decide on `trait_key`; the table is not
            // consulted here, so no site is recorded for the double's header.
            trait_ref: None,
            module,
            trait_name: Some(get_type_name_static(&trait_type)),
            trait_type: Some(trait_type),
            ty: self_type,
            type_params,
            methods: vec![],
            associated_types: vec![],
            is_synthesize_request: false,
            span: dummy_span(),
        }
    }

    /// A position classified on its own, outside any impl: the orphan rule
    /// reads the self type and each trait argument through the same call, so
    /// the tests below exercise it through a header with no type parameters
    /// unless one is named.
    fn classify(ty: &Type, type_params: &[&str], local: &LocalDecls) -> PositionKind {
        let params: Vec<GenericParam> = type_params.iter().map(|n| type_param(n)).collect();
        let header = impl_header(params, named("ForeignTrait"), ty.clone());
        classify_position(ty, &header, local, &resolve_for_test)
    }

    // --- is_user_local ---

    #[test]
    fn test_is_user_local_entry_point() {
        let mut interner = ModuleSourceInterner::new();
        assert!(is_user_local(&interner.entry_point("main.wado")));
    }

    #[test]
    fn test_is_user_local_local_path() {
        let mut interner = ModuleSourceInterner::new();
        assert!(is_user_local(&interner.local("./lib.wado")));
    }

    #[test]
    fn test_is_user_local_core_is_foreign() {
        assert!(!is_user_local(&ModuleSource::prelude()));
    }

    #[test]
    fn test_is_user_local_wasi_is_foreign() {
        assert!(!is_user_local(&ModuleSource::wasi_cli()));
    }

    #[test]
    fn test_is_user_local_remote_is_foreign() {
        let mut interner = ModuleSourceInterner::new();
        assert!(!is_user_local(
            &interner.remote("https://example.com/lib.wado")
        ));
    }

    // --- classify_position ---

    #[test]
    fn test_classify_local_named_type() {
        let tdx = make_local_decls(&["MyError"]);
        assert!(matches!(
            classify(&named("MyError"), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_foreign_named_type() {
        let tdx = make_local_decls(&[]);
        assert!(matches!(
            classify(&named("String"), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_primitive_is_foreign() {
        let tdx = make_local_decls(&[]);
        assert!(matches!(
            classify(&named("i32"), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_uncovered_type_param() {
        let tdx = make_local_decls(&[]);
        assert!(matches!(
            classify(&named("T"), &["T"], &tdx),
            PositionKind::UncoveredTypeParam
        ));
    }

    #[test]
    fn test_classify_undeclared_name_is_foreign_not_uncovered() {
        // A name that reaches no declaration is foreign — the package does not
        // own it, so an inherent `impl` on it is a coherence violation. Reading
        // it as an uncovered parameter (both are `ImplTargetKey::TypeParam`)
        // silently drops that error.
        let tdx = make_local_decls(&["LocalType"]);
        assert!(matches!(
            classify(&named("Undeclared"), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_binder_shadowing_an_undeclared_name_is_uncovered() {
        // The binder question is about the impl's own parameters, so a name
        // both declared nowhere and bound here is still uncovered.
        let tdx = make_local_decls(&[]);
        assert!(matches!(
            classify(&named("Undeclared"), &["Undeclared"], &tdx),
            PositionKind::UncoveredTypeParam
        ));
    }

    #[test]
    fn test_classify_local_generic_head_is_local() {
        // LocalType<T> — head is local regardless of args
        let tdx = make_local_decls(&["LocalType"]);
        let ty = generic("LocalType", vec![named("T")]);
        assert!(matches!(
            classify(&ty, &["T"], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_foreign_generic_is_foreign() {
        // List<T> — head List is foreign
        let tdx = make_local_decls(&[]);
        let ty = generic("List", vec![named("T")]);
        assert!(matches!(
            classify(&ty, &["T"], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_reference_to_local_is_local() {
        // &LocalType — fundamental: look through &
        let tdx = make_local_decls(&["MyStruct"]);
        assert!(matches!(
            classify(&ref_type(named("MyStruct")), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_mut_reference_to_local_is_local() {
        // &mut LocalType — fundamental: look through &mut
        let tdx = make_local_decls(&["MyStruct"]);
        assert!(matches!(
            classify(&mut_ref_type(named("MyStruct")), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_reference_to_foreign_is_foreign() {
        let tdx = make_local_decls(&[]);
        assert!(matches!(
            classify(&ref_type(named("String")), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_tuple_is_foreign() {
        // Tuple types have no single named head → foreign
        let tdx = make_local_decls(&["MyStruct"]);
        let ty = Type::Tuple(vec![named("MyStruct"), named("i32")]);
        assert!(matches!(
            classify(&ty, &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    // --- check_orphan_rfc2451 ---

    #[test]
    fn test_rfc2451_local_self_type_allowed() {
        // impl ForeignTrait for LocalType → T0 is local → allowed
        let tdx = make_local_decls(&["MyStruct"]);
        let ib = impl_header(vec![], named("ForeignTrait"), named("MyStruct"));
        assert!(check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_both_foreign_forbidden() {
        // impl Eq for String → both foreign
        let tdx = make_local_decls(&[]);
        let ib = impl_header(vec![], named("Eq"), named("String"));
        assert!(!check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_local_in_trait_arg_allowed() {
        // impl From<MyError> for String → T0=String(foreign), T1=MyError(local) → allowed
        let tdx = make_local_decls(&["MyError"]);
        let ib = impl_header(
            vec![],
            generic("From", vec![named("MyError")]),
            named("String"),
        );
        assert!(check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_uncovered_type_param_forbidden() {
        // impl<T> Eq for T → T0=T(uncovered) → forbidden
        let tdx = make_local_decls(&[]);
        let ib = impl_header(vec![type_param("T")], named("Eq"), named("T"));
        assert!(!check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_uncovered_param_before_local_in_trait_arg_forbidden() {
        // impl<T> From<T> for String → T0=String(foreign), T1=T(uncovered) → forbidden
        let tdx = make_local_decls(&[]);
        let ib = impl_header(
            vec![type_param("T")],
            generic("From", vec![named("T")]),
            named("String"),
        );
        assert!(!check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_local_type_as_generic_head_in_trait_arg() {
        // impl<T> From<LocalType<T>> for ForeignType → T0=ForeignType, T1=LocalType<T>(local head) → allowed
        let tdx = make_local_decls(&["LocalType"]);
        let trait_ty = generic("From", vec![generic("LocalType", vec![named("T")])]);
        let ib = impl_header(vec![type_param("T")], trait_ty, named("ForeignType"));
        assert!(check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_foreign_generic_head_in_trait_arg_forbidden() {
        // impl<T> From<List<T>> for ForeignType → T0=ForeignType, T1=List<T>(foreign head) → forbidden
        let tdx = make_local_decls(&[]);
        let trait_ty = generic("From", vec![generic("List", vec![named("T")])]);
        let ib = impl_header(vec![type_param("T")], trait_ty, named("ForeignType"));
        assert!(!check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_ref_to_local_as_self_type() {
        // impl ForeignTrait for &LocalType → fundamental, look through & → allowed
        let tdx = make_local_decls(&["MyStruct"]);
        let ib = impl_header(vec![], named("ForeignTrait"), ref_type(named("MyStruct")));
        assert!(check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_ref_to_foreign_as_self_type_forbidden() {
        // impl ForeignTrait for &String → &String is foreign → forbidden
        let tdx = make_local_decls(&[]);
        let ib = impl_header(vec![], named("ForeignTrait"), ref_type(named("String")));
        assert!(!check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_local_self_before_uncovered_param_in_trait_arg() {
        // impl<T> From<T> for LocalType → T0=LocalType(local!) → allowed before reaching T1=T
        let tdx = make_local_decls(&["LocalType"]);
        let ib = impl_header(
            vec![type_param("T")],
            generic("From", vec![named("T")]),
            named("LocalType"),
        );
        assert!(check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }

    #[test]
    fn test_rfc2451_undeclared_self_type_does_not_cover_a_local_trait_arg() {
        // impl From<LocalType> for Undeclared → T0 is foreign (not uncovered),
        // so T1=LocalType still covers the impl. Classifying T0 as uncovered
        // invents an orphan violation for a program whose only real error is
        // the unknown name.
        let tdx = make_local_decls(&["LocalType"]);
        let ib = impl_header(
            vec![],
            generic("From", vec![named("LocalType")]),
            named("Undeclared"),
        );
        assert!(check_orphan_rfc2451(&ib, &tdx, &resolve_for_test));
    }
}
