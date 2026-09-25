//! Global trait knowledge base: trait declarations, impl blocks, and blanket impls.
//!
//! `TraitEnv` is built once before resolution begins and is immutable thereafter.
//! It provides O(1) lookup of trait implementations by type name and trait name,
//! replacing linear scans across all modules.

use std::borrow::Borrow;
use std::sync::Arc;

use crate::ast::{self, AstVisitor, Item, Module, Type};
use crate::defs::{DefId, DefTable};
use crate::elaborator::sig::Signatures;
use crate::elaborator::written::binder_of;
use crate::hashmap::{IndexMap, IndexSet};
use crate::kiln::InvocationIndex;
use crate::loader::resolve_use_decl_source;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name;
use crate::resolve::{Resolution, Resolutions, head_site};
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;
use crate::unparse::unparse_type_into;

/// Namespace-import alias (`use ns from "…"`) → the namespace's module.
/// Drives `ns::Type` resolution (issue #1415).
pub(crate) type NamespaceImports = IndexMap<String, ModuleSource>;

/// Which module each of `module`'s namespace aliases stands for.
///
/// The one import fact the symbol table does not record — it registers the
/// members, not the alias. What a *name* means is [`crate::resolve`]'s answer
/// and is not asked here.
pub(super) fn namespace_imports_of(
    interner: &mut ModuleSourceInterner,
    module: &Module,
    from_module: &ModuleSource,
    entry_module: Option<&ModuleSource>,
    invocations: &InvocationIndex,
) -> NamespaceImports {
    let mut out = NamespaceImports::default();
    for item in &module.items {
        if let Item::Use(use_decl) = item {
            let namespaces = use_decl.items.iter().filter_map(|use_item| match use_item {
                ast::UseItem::Namespace { name: ns } => Some(ns),
                ast::UseItem::Simple { .. }
                | ast::UseItem::InterfaceFunctions { .. }
                | ast::UseItem::Wildcard => None,
            });
            for ns in namespaces {
                let source = resolve_use_decl_source(
                    interner,
                    from_module,
                    use_decl,
                    entry_module,
                    invocations,
                );
                out.insert(ns.clone(), source);
            }
        }
    }
    out
}

use super::types::TypeError;

/// Pick a `ModuleSource` from the AST and synthesised candidate lists: a
/// `prefer` hint wins wherever it appears, else the first AST entry, else the
/// first synthesised one. AST-first is load-bearing — where a type has both a
/// written `impl` and generated code, the written block is the answer — and the
/// union keeps one layer from masking the other on a shared key.
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

/// Identity of an impl's target type. A named type keys by the declaration it
/// names, so two modules' same-named structs — and one type reached under an
/// alias — are the same key exactly when they are the same declaration. A
/// `&T` / `&mut T` target is universal and declares nothing, so it keys by
/// reference kind alone.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ImplTargetKey {
    Decl(DefId),
    /// A head that reaches no declaration: an anonymous or synthetic struct
    /// shape, or a written name that resolves to nothing. Its rendering is all
    /// the identity there is — for a shape that is exactly right, since the
    /// type table interns it under the same pair and two literals of one shape
    /// are one type on purpose; for an unresolved name the writing module is
    /// the only vantage left.
    Undeclared(ModuleSource, String),
    Ref(name::RefKind),
    /// A blanket impl's bare type parameter (`impl<T> Display for T`). It
    /// names no declaration, so it gets no `DefId`: a lookup starts from a
    /// type and can never reach this variant, which is what keeps a blanket
    /// impl out of the bucket of a type that happens to share the parameter's
    /// name. The module is the impl's own — the parameter is scoped to it.
    TypeParam(ModuleSource, String),
    /// A builtin shape: a primitive, `Array`, the tuple family, a function type.
    /// Every mangler spells it bare, so a definition and a lookup agree on it.
    Builtin(String),
}

impl ImplTargetKey {
    /// The key for a declaration already identified. A builtin shape drops its
    /// declaration, as in [`name::FqTypeName::of_head`].
    pub(crate) fn of_decl(defs: &DefTable, def: DefId) -> Self {
        if name::is_builtin_shape_decl(defs, def) {
            return ImplTargetKey::Builtin(defs.name(def).to_string());
        }
        ImplTargetKey::Decl(def)
    }

    /// The key for a head that reaches no declaration: a builtin shape spelled
    /// bare, or a name that resolves to nothing.
    pub(crate) fn of_undeclared(module: &ModuleSource, name: &str) -> Self {
        if name::is_builtin_shape_name(name) {
            return ImplTargetKey::Builtin(name.to_string());
        }
        ImplTargetKey::Undeclared(module.clone(), name.to_string())
    }

    /// The receiver this target indexes under. Built from the same declaration
    /// `TypeTable::impl_receiver_key` reads off a resolved type, so a
    /// definition and a lookup agree by construction.
    pub(crate) fn receiver(&self, defs: &DefTable) -> name::Receiver {
        match self {
            ImplTargetKey::Decl(def) => name::Receiver::Type(name::FqTypeName::of_head(defs, *def)),
            ImplTargetKey::Undeclared(module, name) => {
                name::Receiver::Type(name::FqTypeName::shape(module, name))
            }
            // Not a binder: the bucket every impl in this module binding this
            // spelling as its own parameter shares.
            ImplTargetKey::TypeParam(module, name) => {
                name::Receiver::Type(name::FqTypeName::param_bucket(module, name))
            }
            ImplTargetKey::Ref(kind) => name::Receiver::Ref(*kind),
            ImplTargetKey::Builtin(name) => name::Receiver::Type(name::FqTypeName::builtin(name)),
        }
    }

    pub(crate) fn type_name<'a>(&'a self, defs: &'a DefTable) -> Option<&'a str> {
        match self {
            ImplTargetKey::Decl(def) => Some(defs.name(*def)),
            ImplTargetKey::Undeclared(_, name)
            | ImplTargetKey::TypeParam(_, name)
            | ImplTargetKey::Builtin(name) => Some(name),
            ImplTargetKey::Ref(_) => None,
        }
    }

    /// How to spell this target in a diagnostic — the declaration name, or the
    /// reference prefix for a `&T` / `&mut T` target.
    pub(crate) fn display_name<'a>(&'a self, defs: &'a DefTable) -> &'a str {
        match self {
            ImplTargetKey::Decl(def) => defs.name(*def),
            ImplTargetKey::Undeclared(_, name)
            | ImplTargetKey::TypeParam(_, name)
            | ImplTargetKey::Builtin(name) => name,
            ImplTargetKey::Ref(kind) => kind.prefix(),
        }
    }
}

/// The spelling a declaration renders to in a mangled head: its declared name,
/// with a function-local declaration's disambiguator applied.
///
/// [`crate::tir::TypeTable::decl_render_name`] one layer down, for the callers
/// that hold a [`crate::defs::DefTable`] and no type table.
pub(crate) fn render_decl_name(defs: &DefTable, def: DefId) -> String {
    if defs.is_function_local(def) {
        return name::mangle_local_item_name(defs.name(def), defs.ast_id(def));
    }
    defs.name(def).to_string()
}

/// Target type → the trait impl blocks written for it. Built once from all
/// loaded modules so a method call costs a lookup rather than a scan.
pub(super) type TraitImplIndex = IndexMap<ImplTargetKey, Vec<DefId>>;

type ReceiverImplIndex = IndexMap<name::Receiver, Vec<DefId>>;

fn index_by_receiver(index: &TraitImplIndex, defs: &DefTable) -> ReceiverImplIndex {
    let mut out: ReceiverImplIndex = IndexMap::default();
    for (key, entries) in index {
        out.entry(key.receiver(defs))
            .or_default()
            .extend(entries.iter().copied());
    }
    out
}

/// The trait an `impl` block names, whole: one value, so the identity, the
/// spelling and the arguments cannot disagree.
#[derive(Clone, Debug)]
pub(super) struct ImplTraitRef {
    /// The trait declaration (WEP 2026-08-12); `None` for a trait position
    /// whose site names none.
    pub(super) def: Option<DefId>,
    /// Identity of the trait as the impl indices key it.
    pub(super) key: ImplTargetKey,
    /// The head as written. A spelling, not an identity — compare
    /// [`Self::key`] when the question is *which* trait this is.
    pub(super) name: String,
    /// The reference as written (`Index<K>` in `impl Index<K> for Map`).
    pub(super) ty: Type,
    /// Identity of each argument written, `Self` meaning
    /// [`ImplHeader::target_id`].
    pub(super) arg_ids: Vec<name::FqTypeName>,
}

/// Digested header of an `impl` block, pre-extracted at [`TraitEnv::build`]
/// time so trait/method queries read its trait name, target type, methods,
/// and type parameters without re-fetching the impl block from
/// `loaded_modules`. Keyed by the block's [`DefId`] in
/// [`TraitEnv::impl_headers`].
#[derive(Clone, Debug)]
pub(super) struct ImplHeader {
    /// The module that wrote this header — the vantage every name in
    /// [`Self::ty`] and the trait reference is spelled from. Without it a
    /// consumer holding the header alone can only compare spellings, which is
    /// what makes two modules' same-named types look like one.
    pub(super) module: ModuleSource,
    /// Identity of the impl target, resolved once from [`Self::module`]'s
    /// vantage. The key every impl index in this file is keyed by, so a
    /// whole-program check compares identities rather than written heads.
    pub(super) target: ImplTargetKey,
    /// The trait this header implements; `None` for an inherent
    /// `impl Type { … }` block.
    pub(super) trait_: Option<ImplTraitRef>,
    /// Identity of the impl target, resolved the same way. This is what a
    /// `Self` default on the trait says at this impl.
    pub(super) target_id: name::FqTypeName,
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
    /// Whether the block writes no type parameters. A concrete block hosts its
    /// own function; a generic one's instance is materialised in the
    /// receiver's module.
    pub(super) fn is_concrete(&self) -> bool {
        self.type_params.is_empty()
    }

    /// Whether the target reaches every instance of its head: its arguments,
    /// if any, are distinct parameters of the block.
    pub(super) fn covers_every_instance(&self) -> bool {
        let args = match &self.ty {
            Type::Named(_) => return true,
            Type::Generic(g) => &g.args,
            Type::NamespacedGeneric(g) => &g.args,
            _ => return false,
        };
        let mut seen = IndexSet::default();
        args.iter().all(|arg| {
            let name = match arg {
                Type::Named(n) => &n.name,
                Type::TypePackSpread(name, _) => name,
                _ => return false,
            };
            self.type_params.iter().any(|p| &p.name == name) && seen.insert(name.clone())
        })
    }

    /// Whether the block writes a trait at all, whatever it resolves to.
    pub(super) fn is_trait_impl(&self) -> bool {
        self.trait_.is_some()
    }

    /// `None` for an inherent block, and for a trait position naming no
    /// declaration.
    pub(super) fn trait_def(&self) -> Option<DefId> {
        self.trait_.as_ref()?.def
    }

    pub(super) fn trait_key(&self) -> Option<&ImplTargetKey> {
        Some(&self.trait_.as_ref()?.key)
    }

    pub(super) fn trait_head_name(&self) -> Option<&str> {
        Some(self.trait_.as_ref()?.name.as_str())
    }

    pub(super) fn trait_ty(&self) -> Option<&Type> {
        Some(&self.trait_.as_ref()?.ty)
    }

    pub(super) fn trait_arg_ids(&self) -> &[name::FqTypeName] {
        self.trait_.as_ref().map_or(&[], |t| t.arg_ids.as_slice())
    }
}

/// What fixes one of a blanket impl's type parameters.
#[derive(Clone, Debug)]
pub(crate) enum BlanketParamSource {
    /// The impl's receiver, which the call site's receiver type fills.
    Receiver,
    /// A predicate on another parameter: `..F` in
    /// `impl<S: ReflectStruct<FieldTypes = [..F]>, ..F>`.
    Projection(DefId, String),
    /// A predicate names it, but the bound's site reaches no declaration.
    /// Its own answer: reading it as [`Self::Receiver`] would fill a pack from
    /// the call site's receiver type.
    Unresolved,
}

/// What determines each blanket impl's parameters, in declaration order, keyed
/// by the blanket's `(module, ast_id)`. `None` is the receiver, filled by the
/// call site; `Some((trait, associated type))` is one a predicate fixes. Order
/// is the point — type arguments are consumed positionally, so a receiver
/// written after another parameter sits at a slot the caller never fills.
fn blanket_param_sources(
    impl_headers: &IndexMap<DefId, ImplHeader>,
    blanket_impls: &IndexMap<DefId, Vec<BlanketImpl>>,
    resolutions: &Resolutions,
) -> IndexMap<DefId, Vec<BlanketParamSource>> {
    let mut out: IndexMap<DefId, Vec<BlanketParamSource>> = IndexMap::default();
    for blanket in blanket_impls.values().flatten() {
        let Some(header) = impl_headers.get(&blanket.def) else {
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
                let Some(def) = resolutions.bound_decl(bound) else {
                    return BlanketParamSource::Unresolved;
                };
                BlanketParamSource::Projection(def, assoc.name.clone())
            })
            .collect();
        out.insert(blanket.def, sources);
    }
    out
}

/// Digested signature of a single method inside an [`ImplHeader`]. Holds the
/// name and type parameters method-lookup queries need without the method
/// body; grows field-by-field as consumers migrate off the impl-block AST.
#[derive(Clone, Debug)]
pub(super) struct ImplMethodHeader {
    pub(super) name: String,
    /// The method's own identity — the key into the canonical-signature
    /// digest, so a header lookup reaches the signature without the AST.
    pub(super) def: DefId,
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
    /// Whether the method declares a receiver. Excluded from `param_count`, so
    /// the trait agreement check compares it separately.
    pub(super) has_receiver: bool,
    /// The member's declared rung; consulted only on an inherent impl.
    pub(super) visibility: ast::Visibility,
    pub(super) has_body: bool,
    /// Declared `#[unavailable]`: a name reserved, not a method.
    pub(super) is_reserved: bool,
}

impl ImplMethodHeader {
    /// On a trait declaration, whether every impl owes the method.
    pub(super) fn is_required(&self) -> bool {
        !self.has_body && !self.is_reserved
    }
}

/// Digest each method a `trait` or `impl` block declares. One producer, so the
/// two cannot disagree about what a header says.
fn method_headers(defs: &DefTable, methods: &[ast::Function]) -> Vec<ImplMethodHeader> {
    methods
        .iter()
        .map(|m| ImplMethodHeader {
            name: m.name.clone(),
            def: defs.def_at(m.id),
            type_params: m.type_params.clone(),
            span: m.span,
            name_span: m.name_span,
            param_count: m
                .params
                .iter()
                .filter(|p| p.self_kind == ast::SelfKind::None)
                .count(),
            has_receiver: m.params.iter().any(|p| p.self_kind != ast::SelfKind::None),
            visibility: m.visibility,
            has_body: m.body.is_some(),
            is_reserved: m.unavailable_attr().is_some(),
        })
        .collect()
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
    /// The trait the bound's reference site names, with the arguments it
    /// writes for that trait's own parameters. `None` where the site reaches no
    /// declaration; no argument asks for the declared defaults.
    pub(crate) trait_: Option<name::FqTraitName>,
    /// Associated types the bound pins to the receiver param itself (`Output`
    /// in `T: Mul<Output = T>`) — the only shape decidable against a candidate
    /// receiver; any other right-hand side is the instantiation's to answer.
    pub(crate) pinned_to_receiver: Vec<String>,
}

impl BlanketBound {
    /// The declaration alone, for a question the trait's arguments do not enter.
    pub(crate) fn decl(&self) -> Option<DefId> {
        self.trait_.as_ref()?.canonical()
    }
}

/// A reified blanket impl `impl<Param: Bounds, ..> Trait for <receiver>`.
///
/// The single source of truth for "what kind of blanket is this": the queries
/// that once re-derived it per call site are now selections over this
/// descriptor.
#[derive(Clone, Debug)]
pub(crate) struct BlanketImpl {
    pub(crate) module: ModuleSource,
    /// The impl block's identity, and the key into `impl_headers` for
    /// consumers needing the full header (associated types, bound constraints).
    pub(crate) def: DefId,
    pub(crate) receiver: BlanketReceiver,
    /// Receiver param name (`T` in `impl<T: Bound> Trait for T`).
    pub(crate) param: String,
    /// Bound trait names on the receiver param, each with the declaration its
    /// own reference site resolves to. The spelling stays for the by-name
    /// queries that have not been flipped; the answer is what a bound check
    /// compares, so an aliased bound reaches the trait it aliases.
    pub(crate) bounds: Vec<BlanketBound>,
}

impl BlanketImpl {
    /// Where every template name for this blanket comes from — one built from
    /// the spelling alone looks up a *different* template, silently.
    pub(crate) fn receiver_binder(&self, defs: &DefTable) -> name::FqTypeName {
        name::FqTypeName::binder_of_impl(defs, self.def, &self.param)
    }
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
/// [`TraitEnv::build`] and keyed by the declaration's [`DefId`] in
/// [`TraitEnv::trait_decl_headers`]. Reuses [`ImplMethodHeader`] for the
/// per-method digest (name + type parameters).
#[derive(Clone, Debug)]
pub(super) struct TraitDeclHeader {
    pub(super) name: String,
    /// What each type parameter's declared default says, resolved once from the
    /// trait's own module. One entry per parameter, in declaration order.
    pub(super) default_args: Vec<Option<DefaultArg>>,
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

/// A trait type parameter's declared default (`trait Eq<Rhs = Self>`).
#[derive(Clone, Debug)]
pub(super) enum DefaultArg {
    /// `= Self`, which says the target of whichever impl is answering.
    SelfTarget,
    /// Any other type, whose identity the trait's own module fixes.
    Named(name::FqTypeName),
}

impl DefaultArg {
    fn of(param: &ast::GenericParam, resolutions: &Resolutions) -> Option<Self> {
        match param.default.as_ref()? {
            Type::Named(named) if named.name == "Self" => Some(Self::SelfTarget),
            default => Some(Self::Named(written_type_arg(default, resolutions))),
        }
    }

    /// What it says at an impl whose target is `target`.
    fn at(&self, target: &name::FqTypeName) -> name::FqTypeName {
        match self {
            Self::SelfTarget => target.clone(),
            Self::Named(name) => name.clone(),
        }
    }
}

/// A supertrait paired with the declaration it resolved to. The bound keeps the
/// declaring module's spelling, which need not name the same trait elsewhere.
#[derive(Clone, Debug)]
pub(super) struct InheritedBound {
    /// As the trait that listed it declared it, in that trait's own parameter
    /// space: `trait Foo<A>: Bar<Item = A>` binds `Item` to `Foo`'s `A`.
    pub(super) bound: ast::TraitBound,
    pub(super) decl: DefId,
    /// The clauses leading from the trait owning this closure down to the one
    /// that declared `bound`, outermost first, each written in the previous
    /// one's parameter space. A reader walks them in order, resolving each at
    /// what the step before it answered — never collapsing them to a spelling.
    pub(super) via: Vec<ViaClause>,
}

/// One step of an [`InheritedBound::via`] chain: the clause as its trait wrote
/// it, and the trait it names.
#[derive(Clone, Debug)]
pub(super) struct ViaClause {
    pub(super) bound: ast::TraitBound,
    pub(super) decl: DefId,
}

/// Pre-built index: trait declaration → the transitive closure of its
/// supertraits, deduplicated by declaration and excluding the trait itself. A
/// declared bound `T: Sub` expands through this so `T: Ord` alone carries
/// `Eq`.
pub(super) type SupertraitClosureIndex = IndexMap<DefId, Vec<InheritedBound>>;

/// A method an impl block declares on a type, reachable by name.
#[derive(Clone, Debug)]
pub(super) struct ImplMethodEntry {
    pub(super) name: String,
    /// The declaring module, and an inherent associated function's declared
    /// rung; `None` on a trait impl's.
    pub(super) module: ModuleSource,
    pub(super) inherent_visibility: Option<ast::Visibility>,
    /// Whether the method declares a receiver. `Type::method` names both kinds
    /// and passes the receiver as its first argument; other lookups filter.
    pub(super) has_self: bool,
    /// The method itself: the key into the signature digest, which carries
    /// everything a lookup needs — resolved in the impl's own frame and its
    /// own module's perspective.
    pub(super) method_id: DefId,
}

impl ImplMethodEntry {
    /// Whether an inherent impl declares it. A trait impl's member takes the
    /// trait's reach, and so carries no visibility of its own.
    pub(super) fn is_inherent(&self) -> bool {
        self.inherent_visibility.is_some()
    }
}

/// Every impl block's methods, for O(1) lookup instead of a scan over every
/// module.
///
/// Keyed by canonical declaration key rather than bare type name, so
/// `impl Counter { fn make(...) }` in two modules with same-named
/// `struct Counter` produces two buckets and `CounterA::make(...)` reaches the
/// right one.
pub(super) type ImplMethodIndex = IndexMap<ImplTargetKey, Vec<ImplMethodEntry>>;

/// The static methods a resource declares: `(method_name, module, the owning
/// resource, method_index)` per receiver.
pub(super) type ResourceStaticMethodIndex =
    IndexMap<ImplTargetKey, Vec<(String, ModuleSource, DefId, usize)>>;

/// `(receiver spelling, trait)` → the modules holding that `impl` block, one
/// per module declaring a receiver under that spelling.
pub(crate) type TraitImplModuleIndex = IndexMap<(String, DefId), Vec<ModuleSource>>;

/// Where each `impl <trait> for <type>` lives, reachable from both receiver
/// namespaces.
///
/// The two are not interchangeable — a mangled head (`mod/Widget`) picks out
/// one declaration, a declared name (`Widget`) picks out any declaration
/// spelling itself that way — so they get separate storage and a query answers
/// only from the namespace it named. Storing both in one map is what let a
/// mangled query reach only the synthesised layer and a declared query only the
/// AST layer (WEP 2026-08-12).
#[derive(Debug, Default, Clone)]
pub struct ImplModuleIndex {
    by_mangled: TraitImplModuleIndex,
    by_declared: TraitImplModuleIndex,
}

impl ImplModuleIndex {
    fn get(&self, receiver: ImplReceiver<'_>, trait_: DefId) -> Option<&Vec<ModuleSource>> {
        match receiver {
            ImplReceiver::Of(r) => self.by_mangled.get(&(r.head_key().into_string(), trait_)),
            ImplReceiver::Instantiated(m) => self
                .by_mangled
                .get(&(m.as_mangled_str().to_string(), trait_)),
            ImplReceiver::Declared(d) => {
                self.by_declared.get(&(d.as_decl_str().to_string(), trait_))
            }
        }
    }

    /// Record `module` under both spellings of one receiver identity, so the
    /// two namespaces cannot drift apart.
    pub fn record(&mut self, receiver: &name::Receiver, trait_: DefId, module: &ModuleSource) {
        push_module(
            &mut self.by_mangled,
            receiver.head_key().into_string(),
            trait_,
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
                trait_,
                module,
            );
        }
    }

    /// Record an impl on a generic head under its *instantiated* mangled
    /// receiver (`List<…/Token>`), distinct from the bare head. Mangled-only:
    /// the declaration namespace has no spelling for an instantiation.
    pub fn record_instantiated(&mut self, mangled: String, trait_: DefId, module: &ModuleSource) {
        push_module(&mut self.by_mangled, mangled, trait_, module);
    }
}

fn push_module(
    map: &mut TraitImplModuleIndex,
    receiver: String,
    trait_: DefId,
    module: &ModuleSource,
) {
    let modules = map.entry((receiver, trait_)).or_default();
    if !modules.contains(module) {
        modules.push(module.clone());
    }
}

/// The module each non-blanket `impl` block lives in; `concrete_only` keeps just
/// the parameterless ones, whose substituted calls go to their own module.
fn index_impl_modules(
    impl_headers: &IndexMap<DefId, ImplHeader>,
    defs: &DefTable,
    concrete_only: bool,
) -> ImplModuleIndex {
    let mut out = ImplModuleIndex::default();
    for header in impl_headers.values() {
        // A bodiless derive hosts no code; its generated body registers itself
        // where synthesis lands it.
        if header.is_synthesize_request {
            continue;
        }
        if matches!(header.target, ImplTargetKey::TypeParam(..)) {
            continue;
        }
        if concrete_only && !header.is_concrete() {
            continue;
        }
        let Some(trait_) = header.trait_def() else {
            continue;
        };
        out.record(&header.target.receiver(defs), trait_, &header.module);
    }
    out
}

/// Immutable global knowledge base for trait resolution: pre-built indices over
/// trait impls, declarations, and blanket impls, built once before resolution
/// and shared by `Arc` across every module elaborator. Intentionally not
/// `Clone` — the only mutation is [`Self::extend_with_synthesised`], which moves
/// out of a uniquely-owned `Arc`, so sharing errors instead of deep-cloning.
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
    /// Every declaration in the program. Held here so a query keyed by an
    /// identity can render one for a diagnostic without every caller threading
    /// the table.
    pub(crate) defs: std::sync::Arc<DefTable>,
    /// Digested headers for every indexed impl block, keyed by the block's
    /// [`DefId`]. Trait/method queries read this instead of re-fetching the
    /// impl block AST from `loaded_modules`. See [`ImplHeader`].
    pub(super) impl_headers: IndexMap<DefId, ImplHeader>,
    /// Per blanket impl, what determines each of its parameters, in
    /// declaration order. Resolved once at build time from each bound's own
    /// reference site, so the trait is a declaration rather than the spelling
    /// the blanket wrote (WEP 2026-08-12).
    pub(super) blanket_param_sources: IndexMap<DefId, Vec<BlanketParamSource>>,
    /// Digested headers for every `trait` declaration, keyed by its
    /// [`DefId`]. Lets method-lookup queries read trait
    /// method signatures without re-fetching the trait AST. See
    /// [`TraitDeclHeader`].
    pub(super) trait_decl_headers: IndexMap<DefId, TraitDeclHeader>,
    /// Transitive supertraits per trait declaration. See
    /// [`SupertraitClosureIndex`].
    supertrait_closures: SupertraitClosureIndex,
    /// Free-function type parameters keyed by `(declaring module, function
    /// name)`. Lets `lookup_function_type_params` read a callee's type params
    /// without scanning the module AST.
    pub(super) function_type_params: IndexMap<(ModuleSource, String), Vec<ast::GenericParam>>,
    /// Per-module namespace-import aliases, pre-computed once, so a query
    /// standing in a foreign module's perspective reads them instead of
    /// re-walking its `use` declarations. See [`namespace_imports_of`].
    pub(super) module_namespace_imports: IndexMap<ModuleSource, NamespaceImports>,
    /// Which module each `AstIdSpace` was parsed from, so any node says where
    /// it was written — the module it resolves in however far its AST travels.
    ///
    /// Rebuilt per load: a re-parse mints a new space.
    space_modules: IndexMap<ast::AstIdSpace, ModuleSource>,
    /// Blanket impls by the trait they implement, in registration order. The
    /// single classification source for blanket dispatch (module, receiver
    /// kind, param, bounds), and where the monomorphizer finds the home module
    /// of a generic dispatch the receiver wrote no impl for.
    ///
    /// Keyed by declaration: a name-keyed bucket merged two modules'
    /// same-named traits, so one module's blanket answered the other's bound
    /// (WEP 2026-08-12).
    pub(super) blanket_impls: IndexMap<DefId, Vec<BlanketImpl>>,
    /// Every impl block's methods, by receiver. See [`ImplMethodIndex`].
    pub(super) impl_method_index: ImplMethodIndex,
    /// A resource declaration's receiver-less methods, by receiver. Its
    /// instance methods answer from the declaration itself.
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
        trait_: DefId,
        module: &ModuleSource,
        is_concrete: bool,
    ) {
        if is_concrete {
            self.concrete_trait_impl_modules
                .record(receiver, trait_, module);
        }
        self.trait_impl_modules.record(receiver, trait_, module);
    }

    /// Record a concrete impl on a generic head (`impl Tag for List<Token>`)
    /// under its instantiated receiver, apart from the shared head (#1348).
    pub fn record_instantiation(&mut self, mangled: String, trait_: DefId, module: &ModuleSource) {
        self.concrete_trait_impl_modules
            .record_instantiated(mangled.clone(), trait_, module);
        self.trait_impl_modules
            .record_instantiated(mangled, trait_, module);
    }
}

impl TraitEnv {
    /// The entry `key`'s resource declares for a static named `method_name`.
    pub(super) fn resource_static(
        &self,
        key: &ImplTargetKey,
        method_name: &str,
    ) -> Option<&(String, ModuleSource, DefId, usize)> {
        self.resource_static_method_index
            .get(key)?
            .iter()
            .find(|(name, ..)| name == method_name)
    }

    /// Build the trait indices from all loaded modules, once, before per-module
    /// resolution begins, and check the orphan rule on local impl blocks. Every
    /// receiver-type and trait-name reference in an `impl` header is resolved
    /// from the module that wrote it, so two modules' same-named traits produce
    /// distinct [`DefId`]s.
    pub(super) fn build(
        modules: &IndexMap<ModuleSource, Module>,
        interner: &mut ModuleSourceInterner,
        entry_module: Option<&ModuleSource>,
        invocations: &InvocationIndex,
        resolutions: &Resolutions,
    ) -> (Arc<Self>, Vec<(ModuleSource, TypeError)>) {
        let mut module_namespace_imports: IndexMap<ModuleSource, NamespaceImports> =
            IndexMap::default();
        let mut space_modules: IndexMap<ast::AstIdSpace, ModuleSource> = IndexMap::default();
        for (module_source, module) in modules {
            module_namespace_imports.insert(
                module_source.clone(),
                namespace_imports_of(interner, module, module_source, entry_module, invocations),
            );
            let claimed = space_modules.insert(module.ast_id_space(), module_source.clone());
            assert!(
                claimed.is_none(),
                "one module is one `AstIdSpace`, but {module_source} shares one with {}",
                claimed.expect("just checked")
            );
        }
        let mut impl_index: TraitImplIndex = IndexMap::default();
        let mut all_impl_index: TraitImplIndex = IndexMap::default();
        let mut blanket_impls: IndexMap<DefId, Vec<BlanketImpl>> = IndexMap::default();
        let mut impl_headers: IndexMap<DefId, ImplHeader> = IndexMap::default();
        let mut trait_decl_headers: IndexMap<DefId, TraitDeclHeader> = IndexMap::default();
        let mut function_type_params: IndexMap<(ModuleSource, String), Vec<ast::GenericParam>> =
            IndexMap::default();
        // Every type declaration, for the orphan rule's "does this package own
        // it?" check. A declaration, so a user type shadowing a stdlib name
        // cannot vouch for the stdlib type it shadows.
        let mut type_decl_index: IndexSet<DefId> = IndexSet::default();

        let mut impl_method_index: ImplMethodIndex = IndexMap::default();
        let mut resource_static_method_index: ResourceStaticMethodIndex = IndexMap::default();

        // Pass 1: the declaration-side indices, which pass 2's impl blocks read.
        let defs = resolutions.defs();
        for (module_source, module) in modules {
            for item in &module.items {
                match item {
                    Item::Resource(resource) => {
                        let resource_key = defs.def_at(resource.id);
                        let is_resource = |ty: &ast::Type| {
                            matches!(ty, ast::Type::Named(n)
                                if n.name == "Self" || resolutions.declared(n.id) == Some(resource_key))
                        };
                        for (method_idx, method) in resource.methods.iter().enumerate() {
                            let has_self = method.params.iter().any(|p| match &p.ty {
                                ast::Type::Reference(r) | ast::Type::MutReference(r) => {
                                    is_resource(r)
                                }
                                ty => is_resource(ty),
                            });
                            if !has_self {
                                resource_static_method_index
                                    .entry(ImplTargetKey::Decl(resource_key))
                                    .or_default()
                                    .push((
                                        method.name.clone(),
                                        module_source.clone(),
                                        resource_key,
                                        method_idx,
                                    ));
                            }
                        }
                    }
                    Item::Struct(_)
                    | Item::Variant(_)
                    | Item::Enum(_)
                    | Item::Flags(_)
                    | Item::Newtype(_)
                    | Item::BuiltinTypeDecl(_)
                    | Item::TupleTypeDecl(_) => {
                        type_decl_index.insert(defs.def_at(item.id()));
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
                if let Item::Function(f) = item {
                    function_type_params.insert(
                        (module_source.clone(), f.name.clone()),
                        f.type_params.clone(),
                    );
                }
                if let Item::Trait(trait_decl) = item {
                    trait_decl_headers.insert(
                        defs.def_at(trait_decl.id),
                        TraitDeclHeader {
                            name: trait_decl.name.clone(),
                            default_args: trait_decl
                                .type_params
                                .iter()
                                .map(|p| DefaultArg::of(p, resolutions))
                                .collect(),
                            type_params: trait_decl.type_params.clone(),
                            supertraits: trait_decl.supertraits.clone(),
                            methods: method_headers(defs, &trait_decl.methods),
                            assoc_types: trait_decl.associated_types.clone(),
                            span: trait_decl.span,
                        },
                    );
                    continue;
                }
                let Item::Impl(impl_block) = item else {
                    continue;
                };
                let impl_def = defs.def_at(impl_block.id);
                let type_key = impl_target_key_at(&impl_block.ty, module_source, resolutions);
                let trait_ref = impl_block
                    .trait_type
                    .as_ref()
                    .and_then(|t| resolutions.head_decl(t));
                let trait_ = impl_block
                    .trait_type
                    .as_ref()
                    .map(|trait_type| ImplTraitRef {
                        def: trait_ref,
                        key: trait_ref.map_or_else(
                            || impl_target_key_at(trait_type, module_source, resolutions),
                            ImplTargetKey::Decl,
                        ),
                        name: get_type_name_static(trait_type),
                        ty: trait_type.clone(),
                        arg_ids: written_arg_nodes(trait_type)
                            .iter()
                            .map(|arg| {
                                let node = match arg {
                                    Type::Named(named) if named.name == "Self" => &impl_block.ty,
                                    _ => arg,
                                };
                                written_type_arg(node, resolutions)
                            })
                            .collect(),
                    });
                impl_headers.insert(
                    impl_def,
                    ImplHeader {
                        module: module_source.clone(),
                        target: type_key.clone(),
                        trait_,
                        target_id: written_type_arg(&impl_block.ty, resolutions),
                        ty: impl_block.ty.clone(),
                        type_params: impl_block.type_params.clone(),
                        methods: method_headers(defs, &impl_block.methods),
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
                    .push(impl_def);
                if impl_block.trait_type.is_some() {
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
                                        trait_: resolutions.bound_decl(b).map(|decl| {
                                            name::FqTraitName::declared(resolutions.defs(), decl)
                                                .with_args(
                                                    b.type_args
                                                        .iter()
                                                        .map(|arg| {
                                                            written_type_arg(arg, resolutions)
                                                        })
                                                        .collect(),
                                                )
                                        }),
                                        pinned_to_receiver: b
                                            .assoc_types
                                            .iter()
                                            .filter(|c| get_type_name_static(&c.ty) == param)
                                            .map(|c| c.name.clone())
                                            .collect(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        // A blanket whose trait reference reaches no
                        // declaration answers no bound, so it is not indexed.
                        if let Some(implemented) = trait_ref {
                            blanket_impls
                                .entry(implemented)
                                .or_default()
                                .push(BlanketImpl {
                                    module: module_source.clone(),
                                    def: impl_def,
                                    receiver,
                                    param,
                                    bounds,
                                });
                        }
                    }
                    impl_index
                        .entry(type_key.clone())
                        .or_default()
                        .push(impl_def);
                }
                // Every method of every block, inherent and trait alike, in one
                // canonical bucket: `Type::method` reaches both kinds.
                let is_trait_impl = impl_block.trait_type.is_some();
                for method in &impl_block.methods {
                    impl_method_index
                        .entry(type_key.clone())
                        .or_default()
                        .push(ImplMethodEntry {
                            name: method.name.clone(),
                            module: module_source.clone(),
                            // A trait impl's member takes the trait's reach.
                            inherent_visibility: (!is_trait_impl).then_some(method.visibility),
                            has_self: method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None),
                            method_id: defs.def_at(method.id),
                        });
                }
            }
        }

        // Every trait declaration, as the whole-program checks below want it.
        let decl_index: IndexSet<DefId> = trait_decl_headers.keys().copied().collect();

        // The one answer to "which declaration does this written name mean?",
        // from the writing module's vantage. Every whole-program check below
        // takes it rather than reading a head off the AST, so no check can
        // fall back to comparing spellings.
        let resolve_written =
            |module: &ModuleSource, ty: &ast::Type, _type_params: &[ast::GenericParam]| {
                impl_target_key_at(ty, module, resolutions)
            };

        let mut violations = check_all_orphan_rules(
            defs,
            &impl_headers,
            &decl_index,
            &type_decl_index,
            &resolve_written,
        );

        // The bound's own site says which trait it names, so an aliased
        // supertrait (`use { Base as B }; trait Extra: B`) keys on `Base`'s
        // declaration without the import scope being consulted a second time.
        let resolve_trait = |bound: &ast::TraitBound| {
            let key = resolutions.bound_decl(bound)?;
            decl_index.contains(&key).then_some(key)
        };
        let trait_impl_modules = index_impl_modules(&impl_headers, defs, false);
        let concrete_trait_impl_modules = index_impl_modules(&impl_headers, defs, true);

        violations.extend(check_impl_coherence(&impl_headers, resolutions));
        violations.extend(check_variadic_impl_overlap(defs, &impl_headers));

        let (supertrait_closures, cycles) =
            build_supertrait_closures(defs, &trait_decl_headers, &resolve_trait);
        violations.extend(cycles);
        violations.extend(check_bounds_name_traits(modules, &resolve_trait));

        (
            Arc::new(Self {
                by_receiver: index_by_receiver(&impl_index, defs),
                all_by_receiver: index_by_receiver(&all_impl_index, defs),
                impl_index,
                all_impl_index,
                defs: resolutions.defs().clone(),
                blanket_param_sources: blanket_param_sources(
                    &impl_headers,
                    &blanket_impls,
                    resolutions,
                ),
                impl_headers,
                trait_decl_headers,
                supertrait_closures,
                function_type_params,
                module_namespace_imports,
                space_modules,
                blanket_impls,
                impl_method_index,
                resource_static_method_index,
                trait_impl_modules,
                concrete_trait_impl_modules,
                synthesised: None,
            }),
            violations,
        )
    }

    /// The module `space` was parsed from, or `None` for a synthesized node,
    /// which no module wrote.
    pub(super) fn module_of_space(&self, space: ast::AstIdSpace) -> Option<&ModuleSource> {
        self.space_modules.get(&space)
    }

    /// The pre-computed namespace aliases for `module`. Every loaded module has
    /// a table, empty where it wrote no `use ns from "..."`.
    pub(super) fn namespace_imports(&self, module: &ModuleSource) -> Option<&NamespaceImports> {
        self.module_namespace_imports.get(module)
    }

    /// Every trait's closure, each written in that trait's own parameter space,
    /// which is what a caller stating declarations rather than reading a site
    /// wants.
    pub(super) fn supertrait_closures_in_own_space(
        &self,
    ) -> impl Iterator<Item = (&DefId, &Vec<InheritedBound>)> {
        self.supertrait_closures.iter()
    }

    /// The transitive supertraits of the trait `key` names, deduplicated by
    /// declaration and excluding the trait itself. Empty for a trait with no
    /// supertrait clause, and for a name that declares no trait.
    ///
    /// Written in `key`'s own parameter space, so a caller reading an argument
    /// resolves it through [`InheritedBound::via`] rather than here.
    fn supertrait_closure(&self, key: &DefId) -> &[InheritedBound] {
        self.supertrait_closures.get(key).map_or(&[], Vec::as_slice)
    }

    /// Keys of every impl block on `type_key`, in global build order —
    /// inherent and trait alike.
    pub(super) fn all_impl_keys(&self, type_key: &ImplTargetKey) -> Vec<DefId> {
        self.all_impl_index
            .get(type_key)
            .cloned()
            .unwrap_or_default()
    }

    /// Keys of the **inherent** impls on `type_name`, in global build order —
    /// the `trait_name.is_none()` subset of [`Self::all_impl_index`]. Used by
    /// instance-method lookup, which must not treat trait impls as inherent.
    pub(super) fn inherent_impl_keys(&self, type_key: &ImplTargetKey) -> Vec<DefId> {
        self.all_impl_index
            .get(type_key)
            .map(|keys| {
                keys.iter()
                    .filter(|key| {
                        self.impl_headers
                            .get(*key)
                            .is_some_and(|h| h.trait_.is_none())
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `key` names a trait declaration.
    pub(crate) fn declares_trait(&self, key: &DefId) -> bool {
        self.trait_decl_headers.contains_key(key)
    }

    /// The trait `header` implements, named as a bound can reach it. See
    /// [`args_without_declared_defaults`] for why an argument may drop out.
    pub(super) fn fq_trait_of_impl(
        &self,
        header: &ImplHeader,
        resolutions: &Resolutions,
    ) -> Option<name::FqTraitName> {
        let fq = name::FqTraitName::declared(resolutions.defs(), header.trait_def()?)
            .with_args(header.trait_arg_ids().to_vec());
        Some(self.fq_trait_named_by_impl(fq, &header.ty, resolutions))
    }

    /// [`Self::fq_trait_of_impl`] for a caller holding the written trait
    /// reference and the impl's target rather than a built header.
    pub(super) fn fq_trait_named_by_impl(
        &self,
        fq: name::FqTraitName,
        target: &ast::Type,
        resolutions: &Resolutions,
    ) -> name::FqTraitName {
        let written = args_at_impl_target(fq.args().to_vec(), target, resolutions);
        let Some(params) = fq
            .canonical()
            .and_then(|decl| self.decl_header_of(&decl))
            .map(|header| &header.type_params)
        else {
            return fq.with_args(written);
        };
        let args = args_without_declared_defaults(written, Some(target), params, resolutions);
        fq.with_args(args)
    }

    /// [`Self::fq_trait_named_by_impl`] for a bound, which writes its arguments
    /// itself. Naming the two alike is what lets a bound reach its impl.
    pub(super) fn fq_trait_named_by_bound(
        &self,
        fq: name::FqTraitName,
        bound: &ast::TraitBound,
        resolutions: &Resolutions,
    ) -> name::FqTraitName {
        if bound.type_args.is_empty() {
            return fq;
        }
        let Some(params) = fq
            .canonical()
            .and_then(|decl| self.decl_header_of(&decl))
            .map(|header| &header.type_params)
        else {
            return fq;
        };
        let written = bound
            .type_args
            .iter()
            .map(|arg| written_type_arg(arg, resolutions))
            .collect();
        let args = args_without_declared_defaults(written, None, params, resolutions);
        fq.with_args(args)
    }

    /// The trait's declared default at `index` where it names a type. `None`
    /// for a `= Self` default, which says whatever target is answering rather
    /// than a type of its own.
    pub(super) fn named_default_arg(
        &self,
        trait_: DefId,
        index: usize,
    ) -> Option<&name::FqTypeName> {
        match self
            .trait_decl_headers
            .get(&trait_)?
            .default_args
            .get(index)?
        {
            Some(DefaultArg::Named(name)) => Some(name),
            Some(DefaultArg::SelfTarget) | None => None,
        }
    }

    /// The type parameters `trait_` declares, empty for one that declares none
    /// and for a name reaching no declaration.
    pub(super) fn trait_decl_params(&self, trait_: DefId) -> &[ast::GenericParam] {
        self.decl_header_of(&trait_)
            .map_or(&[], |header| header.type_params.as_slice())
    }

    /// How many arguments the impl on `receiver` writes for `trait_`, among
    /// those a bound writing `wanted` reaches.
    pub(crate) fn impl_written_arg_count(
        &self,
        receiver: &name::Receiver,
        trait_: DefId,
        wanted: &[name::FqTypeName],
    ) -> Option<usize> {
        let defaults = &self.decl_header_of(&trait_)?.default_args;
        self.entries_by_receiver(receiver).find_map(|entry| {
            let header = self.impl_headers.get(&entry)?;
            if header.trait_def() != Some(trait_) {
                return None;
            }
            let default_at =
                |index: usize| Some(defaults.get(index)?.as_ref()?.at(&header.target_id));
            let args = header.trait_arg_ids();
            let answers = wanted.iter().enumerate().all(|(i, want)| {
                let Some(effective) = args.get(i).cloned().or_else(|| default_at(i)) else {
                    return false;
                };
                effective.head_only() == want.head_only()
            });
            // The count the impl's own name spells, not every argument
            // written: `impl Add<Cm> for Cm` mangles as a bare `Add`.
            answers.then(|| non_default_named_arg_count(args, &default_at))
        })
    }

    /// The module defining `impl <trait_> for <receiver>`; `None` for a blanket
    /// impl and a receiver no concrete impl represents.
    pub(crate) fn impl_module_for(
        &self,
        receiver: ImplReceiver<'_>,
        trait_: DefId,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        let ast = self.trait_impl_modules.get(receiver, trait_);
        let syn = self
            .synthesised
            .as_ref()
            .and_then(|s| s.trait_impl_modules.get(receiver, trait_));
        pick_module_union(ast, syn, type_module)
    }

    /// Every module defining `impl <trait_> for <receiver>`, the one
    /// [`Self::impl_module_for`] picks first. Generic impls pinning different
    /// arguments may live in different modules, and the receiver's arguments,
    /// not the receiver's head, choose among them.
    pub(crate) fn impl_modules_for(
        &self,
        receiver: ImplReceiver<'_>,
        trait_: DefId,
        type_module: Option<&ModuleSource>,
    ) -> Vec<&ModuleSource> {
        let ast = self.trait_impl_modules.get(receiver, trait_);
        let syn = self
            .synthesised
            .as_ref()
            .and_then(|s| s.trait_impl_modules.get(receiver, trait_));
        let mut modules: Vec<&ModuleSource> = pick_module_union(ast, syn, type_module)
            .into_iter()
            .collect();
        for module in ast.into_iter().chain(syn).flatten() {
            if !modules.contains(&module) {
                modules.push(module);
            }
        }
        modules
    }

    /// Every impl entry whose target *head* matches `receiver`, across all
    /// declaring modules. The keyed lookups are exact; this is the widened
    /// form for callers that cannot canonicalise — monomorphize and synthesis
    /// run after elaboration and hold a type's name without its import
    /// context. Prefer a keyed lookup wherever a module is known.
    pub(crate) fn entries_by_receiver<'a>(
        &'a self,
        receiver: &'a name::Receiver,
    ) -> impl Iterator<Item = DefId> + 'a {
        self.by_receiver
            .get(receiver)
            .into_iter()
            .flat_map(|entries| entries.iter().copied())
    }

    /// Collected form of [`Self::entries_by_receiver`], for callers that need
    /// to iterate the widened match more than once.
    pub(crate) fn entries_by_receiver_vec(&self, receiver: &name::Receiver) -> Vec<DefId> {
        self.entries_by_receiver(receiver).collect()
    }

    /// Whether any impl on `receiver` implements `trait_` with methods.
    pub(crate) fn has_any_methodful_impl_by_receiver(
        &self,
        receiver: &name::Receiver,
        trait_: DefId,
    ) -> bool {
        self.methodful_impls_by_receiver(receiver, trait_)
            .next()
            .is_some()
    }

    /// Every impl on `receiver` that implements `trait_` with methods.
    pub(crate) fn methodful_impls_by_receiver<'a>(
        &'a self,
        receiver: &'a name::Receiver,
        trait_: DefId,
    ) -> impl Iterator<Item = DefId> + 'a {
        self.entries_by_receiver(receiver)
            .filter(move |&entry| self.methodful_header_matches(entry, trait_))
    }

    /// `key` itself when it declares a trait, else `None` — the question the
    /// callers actually ask, phrased as the identity they then compare.
    pub(crate) fn trait_def(&self, key: &DefId) -> Option<DefId> {
        self.declares_trait(key).then_some(*key)
    }

    /// The trait an [`crate::name::FqTraitName`] names, when it names a trait
    /// declaration.
    pub(crate) fn trait_def_of_fq(&self, fq: &name::FqTraitName) -> Option<DefId> {
        self.trait_def(&fq.canonical()?)
    }

    /// [`Self::has_any_methodful_impl_by_receiver`] narrowed to the impls that
    /// reach every instance of `receiver`, and that `module_source` writes
    /// where it is given. A derived body answers wherever no such impl does.
    pub(crate) fn has_covering_methodful_impl_by_receiver(
        &self,
        receiver: &name::Receiver,
        trait_: DefId,
        module_source: Option<&ModuleSource>,
    ) -> bool {
        self.entries_by_receiver(receiver).any(|entry| {
            module_source.is_none_or(|module| self.defs.module(entry) == module)
                && self.methodful_header_matches(entry, trait_)
                && self.impl_headers[&entry].covers_every_instance()
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
                    h.trait_.is_none() && h.methods.iter().any(|m| m.name == method_name)
                })
            })
    }

    fn methodful_header_matches(&self, entry: DefId, trait_: DefId) -> bool {
        self.impl_headers
            .get(&entry)
            .is_some_and(|header| !header.methods.is_empty() && header.trait_def() == Some(trait_))
    }

    /// Return the home module of a *value* blanket (`impl<T: Bound> Trait for
    /// T`) for `trait_name`, if one exists — `value_blanket_for_trait` excludes
    /// ref blankets, so a `impl<T: Inspect> Inspect for &T` is never returned for
    /// a value receiver. `type_module` is preferred as a stable tie-breaker when
    /// several modules host a value blanket for the trait.
    pub(crate) fn blanket_impl_module_for_trait(
        &self,
        trait_: DefId,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        self.value_blanket_for_trait(trait_, type_module)
            .map(|b| &b.module)
    }

    /// The value blanket for `trait_name` whose receiver-param bounds `satisfies`
    /// accepts. A trait may carry several disjoint value blankets — the four
    /// reflection kinds each derive `Inspect` over their own `Reflect*` bound —
    /// so a receiver-blind first-wins selection would hand every receiver the
    /// first-registered kind and then reject it on the bound check.
    pub(crate) fn value_blanket_for_receiver(
        &self,
        trait_: DefId,
        type_module: Option<&ModuleSource>,
        satisfies: &dyn Fn(&[BlanketBound]) -> bool,
    ) -> Option<&BlanketImpl> {
        let impls = self.blanket_impls.get(&trait_)?;
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
        trait_: DefId,
        type_module: Option<&ModuleSource>,
    ) -> Option<&BlanketImpl> {
        let impls = self.blanket_impls.get(&trait_)?;
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
    pub(crate) fn has_universal_ref_blanket(&self, trait_: DefId, is_mut: bool) -> bool {
        self.blanket_impls.get(&trait_).is_some_and(|impls| {
            impls
                .iter()
                .any(|b| b.receiver == BlanketReceiver::Ref { is_mut })
        })
    }

    /// What determines each of a blanket impl's parameters, in declaration
    /// order — see [`blanket_param_sources`].
    pub(crate) fn blanket_param_sources(&self, blanket: &BlanketImpl) -> Vec<BlanketParamSource> {
        self.blanket_param_sources
            .get(&blanket.def)
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
        trait_: DefId,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        let ast = self.concrete_trait_impl_modules.get(receiver, trait_);
        let syn = self
            .synthesised
            .as_ref()
            .and_then(|s| s.concrete_trait_impl_modules.get(receiver, trait_));
        pick_module_union(ast, syn, type_module)
    }

    /// The module of the concrete impl behind `info`: keyed by its
    /// instantiation, then by the bare head an impl may be written on.
    pub(crate) fn concrete_impl_module_of(
        &self,
        info: &name::LocalMethodName,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        self.impl_module_of(info, type_module, Self::concrete_impl_module_for)
    }

    /// [`Self::concrete_impl_module_of`], generic impls included.
    pub(crate) fn any_impl_module_of(
        &self,
        info: &name::LocalMethodName,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        self.impl_module_of(info, type_module, Self::impl_module_for)
    }

    fn impl_module_of<'a>(
        &'a self,
        info: &name::LocalMethodName,
        type_module: Option<&ModuleSource>,
        lookup: impl Fn(
            &'a Self,
            ImplReceiver<'_>,
            DefId,
            Option<&ModuleSource>,
        ) -> Option<&'a ModuleSource>,
    ) -> Option<&'a ModuleSource> {
        let trait_ = info.trait_decl()?;
        lookup(
            self,
            ImplReceiver::Instantiated(&info.mangled_struct_name()),
            trait_,
            type_module,
        )
        .or_else(|| lookup(self, ImplReceiver::Of(info.receiver()), trait_, type_module))
    }

    /// The digested declaration `key` identifies, or `None` when it names no
    /// trait.
    pub(super) fn decl_header_of(&self, key: &DefId) -> Option<&TraitDeclHeader> {
        self.trait_decl_headers.get(key)
    }

    /// The trait's declaration of the associated type `assoc_name`, or `None`
    /// when `key` names no trait or that trait declares no such type.
    pub(super) fn assoc_type_decl(
        &self,
        key: &DefId,
        assoc_name: &str,
    ) -> Option<&ast::AssociatedTypeDecl> {
        self.decl_header_of(key)?
            .assoc_types
            .iter()
            .find(|decl| decl.name == assoc_name)
    }

    /// Whether the trait `key` identifies declares `assoc_name`.
    pub(super) fn declares_assoc_type(&self, key: &DefId, assoc_name: &str) -> bool {
        self.assoc_type_decl(key, assoc_name).is_some()
    }

    /// The supertrait of `key` declaring `assoc_name`, re-spelled at `written`.
    /// A trait inherits its supertraits' associated types, so `T: Ord` answers
    /// for `Eq`'s.
    pub(super) fn supertrait_declaring_assoc_type(
        &self,
        key: &DefId,
        assoc_name: &str,
    ) -> Option<DefId> {
        self.supertrait_decls(key)
            .find(|decl| self.declares_assoc_type(decl, assoc_name))
    }

    /// Which traits `key`'s transitive supertraits are. The clause's arguments
    /// do not change that, so no caller of this owes them.
    pub(super) fn supertrait_decls(&self, key: &DefId) -> impl Iterator<Item = DefId> + '_ {
        self.supertrait_closure(key)
            .iter()
            .map(|inherited| inherited.decl)
    }

    /// `key`'s parameters and its closure as declared, both in `key`'s own
    /// parameter space. A reader re-spells them at its own arguments with
    /// [`TypeSystem::supertrait_names`].
    pub(super) fn supertrait_closure_declared(
        &self,
        key: &DefId,
    ) -> (&[ast::GenericParam], &[InheritedBound]) {
        (self.trait_decl_params(*key), self.supertrait_closure(key))
    }

    /// `key` or the supertrait of it declaring `assoc_name`, making
    /// `<T as key>::assoc_name` mean the trait that declared it.
    pub(super) fn trait_declaring_assoc_type(
        &self,
        key: &DefId,
        assoc_name: &str,
    ) -> Option<DefId> {
        if self.declares_assoc_type(key, assoc_name) {
            return Some(*key);
        }
        self.supertrait_declaring_assoc_type(key, assoc_name)
    }

    /// Which of `bounds` declares `assoc_name`, making `T::assoc_name` mean
    /// `<T as ThatTrait>::assoc_name`.
    pub(super) fn bound_declaring_assoc_type<B: Borrow<ast::TraitBound>>(
        &self,
        bounds: &[B],
        assoc_name: &str,
        resolutions: &Resolutions,
    ) -> Option<DefId> {
        let decls = || {
            bounds
                .iter()
                .filter_map(|bound| resolutions.bound_decl(bound.borrow()))
        };
        decls()
            .find(|decl| self.declares_assoc_type(decl, assoc_name))
            // Searched after every direct bound, so a trait redeclaring the
            // name still wins for itself.
            .or_else(|| {
                decls().find_map(|decl| self.supertrait_declaring_assoc_type(&decl, assoc_name))
            })
    }

    /// Produce a new `TraitEnv` carrying the synthesis-layer impls — every
    /// `(type_name, trait_name) -> ModuleSource` found in TIR once synthesis has
    /// added its auto-derived impls. Called once per pipeline run; calling again
    /// replaces the layer. `prev` must be the unique owner: extension moves out
    /// of the `Arc`, swaps a field and re-wraps, so a shared `Arc` panics.
    pub fn extend_with_synthesised(prev: Arc<Self>, synth_impls: SynthesisedImpls) -> Arc<Self> {
        let Ok(mut env) = Arc::try_unwrap(prev) else {
            panic!("extend_with_synthesised: TraitEnv Arc must be uniquely owned")
        };
        env.synthesised = Some(synth_impls);
        Arc::new(env)
    }
}

/// Which namespace an impl-module query spells its receiver in. The two are not
/// interchangeable — a mangled fq receiver picks out one declaration, a declared
/// name picks out any declaration spelling itself that way — and each has its
/// own storage, written from one receiver identity, so a query cannot land in
/// the wrong one (WEP 2026-08-12). [`Self::Of`] carries the identity and derives
/// both spellings; the others are for callers holding only one.

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

/// The impl header's target, from the site the header wrote.
///
/// A site behind no declaration — a tuple, a function type, a name that
/// reaches nothing — is keyed to the impl's own module. Nothing else claims
/// it, and coherence for exactly those is decided per module.
fn impl_target_key_at(
    ty: &ast::Type,
    module_source: &ModuleSource,
    resolutions: &Resolutions,
) -> ImplTargetKey {
    sited_impl_target_key(ty, module_source, resolutions)
        .unwrap_or_else(|| ImplTargetKey::of_undeclared(module_source, &get_type_name_static(ty)))
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
    resolutions: &Resolutions,
) -> Option<ImplTargetKey> {
    // A reference target buckets by kind alone: the table resolves `&List<T>`
    // to `List`, which is the referent, not the bucket.
    if let Some(kind) = name::RefKind::from_ast(ty) {
        return Some(ImplTargetKey::Ref(kind));
    }
    let site = head_site(ty)?;
    match resolutions.get(site) {
        // The impl's own binder, which shadows any declaration of that name —
        // `impl<T> Trait for T` written where a `struct T` exists stays a
        // blanket.
        Resolution::Binder(_) => Some(ImplTargetKey::TypeParam(
            module_source.clone(),
            get_type_name_static(ty),
        )),
        Resolution::Def(def) => Some(ImplTargetKey::of_decl(resolutions.defs(), def)),
        Resolution::Projection(_) | Resolution::Unresolved => None,
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
    types: IndexSet<DefId>,
    traits: IndexSet<DefId>,
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
        // Asked of the impl's own binders, not of `ImplTargetKey::TypeParam`,
        // which also covers a name reaching no declaration: reading that as
        // uncovered loses the coherence error `impl Undeclared { … }` deserves
        // and invents an orphan violation for `impl From<Local> for Undeclared`.
        Type::Named(_) | Type::Generic(_) if binder_of(ty, &header.type_params).is_some() => {
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
                | ImplTargetKey::Builtin(_)
                | ImplTargetKey::Undeclared(..) => PositionKind::ForeignType,
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
    let trait_args: &[Type] = match header.trait_ty() {
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
type ResolveTrait<'a> = &'a dyn Fn(&ast::TraitBound) -> Option<DefId>;

/// Resolves a type written in one module — the vantage — to the declaration it
/// names, shadowed by the surrounding item's own type parameters. The single
/// answer to "which type is this name?", handed to whole-program checks so
/// none of them re-derives one from a bare head.
type ResolveWritten<'a> =
    &'a dyn Fn(&ModuleSource, &ast::Type, &[ast::GenericParam]) -> ImplTargetKey;

/// The arguments a bound writes, as written: the key two edges to one trait
/// are the same edge at, so `D<X>` and `D<List<X>>` stay two.
fn written_args_key(bound: &ast::TraitBound) -> String {
    let mut out = String::new();
    for arg in &bound.type_args {
        unparse_type_into(arg, &mut out);
        out.push(',');
    }
    out
}

/// Add an inherited bound unless the list already holds that supertrait at
/// those arguments, reached by the same chain of clauses.
///
/// Two chains may write one clause alike and still reach different arguments,
/// and a step's own trait supplies the defaults for what it leaves out.
fn push_unique_inherited(bounds: &mut Vec<InheritedBound>, bound: &InheritedBound) {
    let identity = |b: &InheritedBound| {
        let chain: Vec<(DefId, String)> = b
            .via
            .iter()
            .map(|step| (step.decl, written_args_key(&step.bound)))
            .collect();
        (written_args_key(&b.bound), chain)
    };
    let key = identity(bound);
    let Some(existing) = bounds
        .iter_mut()
        .find(|b| b.decl == bound.decl && identity(b) == key)
    else {
        bounds.push(bound.clone());
        return;
    };
    if existing.bound.assoc_types.is_empty() && !bound.bound.assoc_types.is_empty() {
        *existing = bound.clone();
    }
}

/// An inherited bound with `direct` prepended to the chain that reaches it:
/// `trait A<X>: B<X>` over `trait B<Y>: C<Y>` records `B<X>` ahead of `C<Y>`,
/// each staying in the space it was written in. `decl` is the trait `direct`
/// names.
fn through_clause(
    inherited: &InheritedBound,
    direct: &ast::TraitBound,
    decl: DefId,
) -> InheritedBound {
    let step = ViaClause {
        bound: direct.clone(),
        decl,
    };
    InheritedBound {
        via: std::iter::once(step)
            .chain(inherited.via.iter().cloned())
            .collect(),
        ..inherited.clone()
    }
}

/// Every written bound that reaches no trait declaration, at the bound. One
/// walk answers for every position a bound can be written in.
fn check_bounds_name_traits(
    modules: &IndexMap<ModuleSource, Module>,
    resolve: ResolveTrait<'_>,
) -> Vec<(ModuleSource, TypeError)> {
    struct Bounds<'a> {
        module: &'a ModuleSource,
        resolve: ResolveTrait<'a>,
        unknown: Vec<(ModuleSource, TypeError)>,
    }
    impl AstVisitor for Bounds<'_> {
        fn visit_trait_bounds(&mut self, bounds: &[ast::TraitBound]) {
            for bound in bounds {
                let written = bound.resolved.is_none() && bound.fn_signature.is_none();
                if written && (self.resolve)(bound).is_none() {
                    self.unknown.push((
                        self.module.clone(),
                        TypeError::UnknownBound {
                            name: bound.name.clone(),
                            span: bound.span,
                        },
                    ));
                }
            }
            ast::walk_trait_bounds(self, bounds);
        }
    }

    let mut unknown = Vec::new();
    for (module_source, module) in modules {
        let mut walk = Bounds {
            module: module_source,
            resolve,
            unknown,
        };
        for item in &module.items {
            walk.visit_item(item);
        }
        unknown = walk.unknown;
    }
    unknown
}

/// Expand every trait's direct supertraits into its transitive closure,
/// reporting each trait that reaches itself. A cycle's edge is cut rather than
/// followed, keeping the closure finite.
fn build_supertrait_closures(
    defs: &DefTable,
    headers: &IndexMap<DefId, TraitDeclHeader>,
    resolve: ResolveTrait<'_>,
) -> (SupertraitClosureIndex, Vec<(ModuleSource, TypeError)>) {
    let mut closures = SupertraitClosureIndex::default();
    if headers.values().all(|h| h.supertraits.is_empty()) {
        return (closures, Vec::new());
    }
    let mut cycles = Vec::new();
    let mut reported: IndexSet<DefId> = IndexSet::default();
    for loc in headers.keys() {
        let mut stack = Vec::new();
        expand_supertraits(
            defs,
            *loc,
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
    defs: &DefTable,
    loc: DefId,
    headers: &IndexMap<DefId, TraitDeclHeader>,
    resolve: ResolveTrait<'_>,
    closures: &mut SupertraitClosureIndex,
    stack: &mut Vec<DefId>,
    reported: &mut IndexSet<DefId>,
    cycles: &mut Vec<(ModuleSource, TypeError)>,
) -> Vec<InheritedBound> {
    if let Some(done) = closures.get(&loc) {
        return done.clone();
    }
    let Some(header) = headers.get(&loc) else {
        return Vec::new();
    };

    stack.push(loc);
    let mut closure: Vec<InheritedBound> = Vec::new();
    for direct in &header.supertraits {
        let Some(super_loc) = resolve(direct) else {
            continue;
        };
        // Before the push: `trait Loop: Loop` must not land in its own closure.
        if let Some(pos) = stack.iter().position(|s| *s == super_loc) {
            report_supertrait_cycle(defs, pos, stack, headers, reported, cycles);
            continue;
        }
        push_unique_inherited(
            &mut closure,
            &InheritedBound {
                bound: direct.clone(),
                decl: super_loc,
                via: Vec::new(),
            },
        );
        for inherited in expand_supertraits(
            defs, super_loc, headers, resolve, closures, stack, reported, cycles,
        ) {
            push_unique_inherited(&mut closure, &through_clause(&inherited, direct, super_loc));
        }
    }
    stack.pop();

    closures.insert(loc, closure.clone());
    closure
}

/// Report the cycle closed by the edge back to `stack[pos]`, attributing it to
/// that trait — the one that turned out to be its own supertrait.
fn report_supertrait_cycle(
    defs: &DefTable,
    pos: usize,
    stack: &[DefId],
    headers: &IndexMap<DefId, TraitDeclHeader>,
    reported: &mut IndexSet<DefId>,
    cycles: &mut Vec<(ModuleSource, TypeError)>,
) {
    let culprit = stack[pos];
    if !reported.insert(culprit) {
        return;
    }
    let Some(header) = headers.get(&culprit) else {
        return;
    };
    let mut chain: Vec<String> = stack[pos..]
        .iter()
        .filter_map(|s| headers.get(s).map(|h| h.name.clone()))
        .collect();
    chain.push(header.name.clone());
    cycles.push((
        defs.module(culprit).clone(),
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

/// The coherence checks the solver owns, given spans and names by the headers
/// they came from. Only a user-local impl is reported: a stdlib pair the check
/// would name is not something a program can fix.
/// How a finding names the impl at `conflict`, reported at `here`.
fn conflicting_impl_location(conflict: &ModuleSource, here: &ModuleSource) -> String {
    if conflict == here {
        "an earlier impl in this file".to_string()
    } else {
        format!("the one in `{conflict}`")
    }
}

fn check_impl_coherence(
    impl_headers: &IndexMap<DefId, ImplHeader>,
    resolutions: &Resolutions,
) -> Vec<(ModuleSource, TypeError)> {
    use super::solver_bridge::{Lowering, lower_impls};
    use crate::trait_solver::{CoherenceError, ImplId, Program, coherence_errors};
    let mut lowering = Lowering::default();
    let mut program = Program::default();
    let sources = lower_impls(&mut lowering, &mut program, impl_headers, resolutions);
    let header_of = |id: ImplId| -> &ImplHeader { sources[id.0 as usize] };
    let trait_name = |header: &ImplHeader| {
        header
            .trait_head_name()
            .expect("a coherence finding names a trait impl")
            .to_string()
    };
    let mut violations = Vec::new();
    for error in coherence_errors(&program) {
        let (reported, error) = match error {
            CoherenceError::DuplicateImpl { first, second } => {
                let (first, second) = (header_of(first), header_of(second));
                (
                    second,
                    TypeError::DuplicateTraitImpl {
                        trait_name: trait_name(second),
                        self_type_name: get_type_name_static(&second.ty),
                        conflicting_impl: conflicting_impl_location(&first.module, &second.module),
                        span: second.span,
                    },
                )
            }
            CoherenceError::UnboundedValueBlanket { impl_ } => {
                let header = header_of(impl_);
                (
                    header,
                    TypeError::UnboundedValueBlanket {
                        trait_name: trait_name(header),
                        param: get_type_name_static(&header.ty),
                        span: header.span,
                    },
                )
            }
        };
        if is_user_local(&reported.module) {
            violations.push((reported.module.clone(), error));
        }
    }
    violations
}

/// Coherence Rule 2 (WEP 2026-03-14 §5): two variadic impls of one trait accept
/// the same tuples, and a pack's bounds resolve only at monomorphization, so
/// nothing separates them at selection — reject the later one where it is
/// written. Grouping is by trait *declaration*, so two modules may each keep
/// their own. The same walk refuses a target the compiler cannot implement.
fn check_variadic_impl_overlap(
    defs: &DefTable,
    impl_headers: &IndexMap<DefId, ImplHeader>,
) -> Vec<(ModuleSource, TypeError)> {
    let mut violations = Vec::new();
    let mut groups: IndexMap<DefId, Vec<VariadicImpl<'_>>> = IndexMap::default();

    for header in impl_headers.values() {
        if !header.is_trait_impl() {
            continue;
        }
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
        let Some(trait_) = header.trait_def() else {
            continue;
        };
        groups.entry(trait_).or_default().push(VariadicImpl {
            module_source: &header.module,
            span: header.span,
            trait_name: defs.name(trait_).to_string(),
            trait_args: match header.trait_ty() {
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
                    conflicting_impl: conflicting_impl_location(
                        conflict.module_source,
                        candidate.module_source,
                    ),
                    span: candidate.span,
                },
            ));
        }
    }

    violations
}

/// The methods an inherent impl defines again for a receiver an earlier
/// inherent impl reaches, which carry no trait contract to agree on.
pub(super) fn inherent_impl_overlaps(
    defs: &DefTable,
    impl_headers: &IndexMap<DefId, ImplHeader>,
    signatures: &Signatures,
    type_table: &TypeTable,
) -> Vec<(ModuleSource, TypeError)> {
    let mut by_target: IndexMap<name::Receiver, Vec<(&ImplHeader, TypeId)>> = IndexMap::default();
    for (def, header) in impl_headers {
        if header.trait_.is_some() {
            continue;
        }
        if let Some(sig) = signatures.impl_sig(*def) {
            // An `impl &T` defines its methods on `T`, so it keys by the pointee.
            let pointee = type_table.peel_refs(sig.target);
            by_target
                .entry(type_table.impl_receiver_key(pointee))
                .or_default()
                .push((header, pointee));
        }
    }
    let mut violations = Vec::new();
    for blocks in by_target.values() {
        for (at, &(later, later_target)) in blocks.iter().enumerate() {
            if !is_user_local(&later.module) {
                continue;
            }
            let mut reported: IndexSet<&str> = IndexSet::default();
            for &(earlier, earlier_target) in &blocks[..at] {
                if !type_table.targets_overlap(earlier_target, later_target) {
                    continue;
                }
                for method in &later.methods {
                    if earlier.methods.iter().any(|m| m.name == method.name)
                        && reported.insert(method.name.as_str())
                    {
                        violations.push((
                            later.module.clone(),
                            TypeError::DuplicateInherentMethod {
                                self_type_name: match &later.ty {
                                    Type::Reference(_) | Type::MutReference(_) => {
                                        written_type_source(&later.ty)
                                    }
                                    _ => later.target.display_name(defs).to_string(),
                                },
                                method_name: method.name.clone(),
                                span: method.span,
                            },
                        ));
                    }
                }
            }
        }
    }
    violations
}

/// Check orphan rules for all trait impl blocks across all modules.
/// Only impl blocks in local (user) modules are checked. Each violation is
/// paired with the offending impl's [`ModuleSource`] for file attribution.
fn check_all_orphan_rules(
    defs: &DefTable,
    impl_headers: &IndexMap<DefId, ImplHeader>,
    decl_index: &IndexSet<DefId>,
    type_decl_index: &IndexSet<DefId>,
    resolve: ResolveWritten<'_>,
) -> Vec<(ModuleSource, TypeError)> {
    let mut violations = Vec::new();

    let owned = |def: &&DefId| is_user_local(defs.module(**def));
    let local = LocalDecls {
        types: type_decl_index.iter().filter(owned).copied().collect(),
        traits: decl_index.iter().filter(owned).copied().collect(),
        tuple: type_decl_index
            .iter()
            .filter(owned)
            .any(|def| defs.name(*def) == TypeTable::TUPLE_TYPE_NAME),
    };

    for header in impl_headers.values() {
        if !is_user_local(&header.module) {
            continue;
        }

        let Some(trait_key) = header.trait_key() else {
            // Inherent impl: the orphan rule does not apply, but coherence does
            // — a package may only define inherent methods on types it owns, or
            // two packages could add colliding methods to `String`. Use a trait
            // instead. `classify_position` looks through references and counts a
            // `LocalType` head as owned, and stdlib modules are skipped above.
            if let PositionKind::ForeignType =
                classify_position(&header.ty, header, &local, resolve)
            {
                violations.push((
                    header.module.clone(),
                    TypeError::InherentImplOnForeignType {
                        self_type_name: header.target.display_name(defs).to_string(),
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
                    trait_name: trait_key.display_name(defs).to_string(),
                    self_type_name: header.target.display_name(defs).to_string(),
                    span: header.span,
                },
            ));
        }
    }

    violations
}

/// The argument nodes a written trait reference carries, empty for a bare
/// name. `ns::Trait<T>` supplies them the same as `Trait<T>` does: the
/// namespace is the head's question, not the argument list's.
pub(super) fn written_arg_nodes(ty: &ast::Type) -> &[ast::Type] {
    match ty {
        ast::Type::Generic(generic) => &generic.args,
        ast::Type::NamespacedGeneric(ns) => &ns.args,
        _ => &[],
    }
}

/// The type arguments a written trait position supplies, each read off the node
/// that wrote it, so its own reference site says which declaration it names.
pub(super) fn written_type_args(
    ty: &ast::Type,
    resolutions: &Resolutions,
) -> Vec<name::FqTypeName> {
    match ty {
        ast::Type::Generic(_) | ast::Type::NamespacedGeneric(_) => written_arg_nodes(ty)
            .iter()
            .map(|arg| written_type_arg(arg, resolutions))
            .collect(),
        _ => Vec::new(),
    }
}

/// A trait argument list with every trailing argument that only restates the
/// declared default dropped, `Self` meaning the impl's own target — so
/// `impl Add<Cm> for Cm` reaches `T: Add` and `impl Add<Inch> for Cm` does not.
fn args_without_declared_defaults(
    written: Vec<name::FqTypeName>,
    target: Option<&ast::Type>,
    params: &[ast::GenericParam],
    resolutions: &Resolutions,
) -> Vec<name::FqTypeName> {
    let kept = non_default_named_arg_count(&written, &|index| {
        declared_default_arg(params, index, target, resolutions)
    });
    let mut written = written;
    written.truncate(kept);
    written
}

/// Whether the header answers a bound writing `wanted`: at every position each
/// side says its written argument, or the declared default where it wrote none.
pub(super) fn header_answers_bound_args(
    written: &[name::FqTypeName],
    target: &ast::Type,
    params: &[ast::GenericParam],
    resolutions: &Resolutions,
    wanted: &[name::FqTypeName],
) -> bool {
    let default_at = |i: usize| declared_default_arg(params, i, Some(target), resolutions);
    (0..written.len().max(wanted.len())).all(|i| {
        // A position the bound leaves open and the trait gives no default is
        // one no bound can name, so every impl answers there.
        let Some(asks) = wanted.get(i).cloned().or_else(|| default_at(i)) else {
            return true;
        };
        written.get(i).cloned().or_else(|| default_at(i)) == Some(asks)
    })
}

/// What the trait's declared default at `index` says, `Self` meaning the impl's
/// target. A bound has no target node, so there `Self` says only itself.
fn declared_default_arg(
    params: &[ast::GenericParam],
    index: usize,
    target: Option<&ast::Type>,
    resolutions: &Resolutions,
) -> Option<name::FqTypeName> {
    let default = params.get(index)?.default.as_ref()?;
    match default {
        ast::Type::Named(named) if named.name == "Self" => Some(match target {
            Some(target) => written_type_arg(target, resolutions),
            None => written_type_arg(default, resolutions),
        }),
        _ => Some(written_type_arg(default, resolutions)),
    }
}

/// How many of `trait_type`'s written arguments say something its declared
/// defaults do not. One rule behind both an impl's name and the identity its
/// associated types register under, so the two cannot disagree.
pub(super) fn non_default_arg_count(
    ast_args: &[ast::Type],
    target: Option<&ast::Type>,
    params: &[ast::GenericParam],
    resolutions: &Resolutions,
) -> usize {
    let written: Vec<name::FqTypeName> = ast_args
        .iter()
        .map(|arg| written_type_arg(arg, resolutions))
        .collect();
    non_default_named_arg_count(&written, &|index| {
        declared_default_arg(params, index, target, resolutions)
    })
}

/// [`non_default_arg_count`] over identities rather than spellings, for a
/// consumer holding the arguments already resolved.
pub(super) fn non_default_named_arg_count(
    args: &[name::FqTypeName],
    default_at: &dyn Fn(usize) -> Option<name::FqTypeName>,
) -> usize {
    let mut kept = args.len();
    while let Some(last) = kept.checked_sub(1) {
        if args.get(last) != default_at(last).as_ref() {
            break;
        }
        kept = last;
    }
    kept
}

/// Written trait arguments with `Self` read as the impl's own target, so
/// `impl Add<Self> for Feet` says `Add<Feet>` wherever an impl head is read.
pub(super) fn args_at_impl_target(
    written: Vec<name::FqTypeName>,
    target: &ast::Type,
    resolutions: &Resolutions,
) -> Vec<name::FqTypeName> {
    let target_id = written_type_arg(target, resolutions);
    written
        .into_iter()
        .map(|arg| arg.substitute(&name::FqTypeName::binder("Self"), &target_id))
        .collect()
}

/// One written type argument as the identity it names.
///
/// A name that reaches no declaration keeps its spelling — there is no identity
/// to hold, and [`name::TypeHead::Builtin`] is the case that says so.
pub(super) fn written_type_arg(ty: &ast::Type, resolutions: &Resolutions) -> name::FqTypeName {
    let nested = |args: &[ast::Type]| -> Vec<name::FqTypeName> {
        args.iter()
            .map(|arg| written_type_arg(arg, resolutions))
            .collect()
    };
    match ty {
        ast::Type::Reference(inner) => {
            written_type_arg(inner, resolutions).with_reference(name::RefKind::Shared)
        }
        ast::Type::MutReference(inner) => {
            written_type_arg(inner, resolutions).with_reference(name::RefKind::Mut)
        }
        ast::Type::Tuple(elems) => name::FqTypeName::tuple(nested(elems)),
        // Spelled by the whole shape, matching the resolved form: the two
        // sides of a lookup have to render one type one way.
        ast::Type::Function(ft) => {
            let params: Vec<String> = ft
                .params
                .iter()
                .map(|param| written_type_arg(param, resolutions).to_mangled())
                .collect();
            let with_clause: Vec<String> = ft
                .effects
                .iter()
                .map(|effect| {
                    resolutions
                        .effect_at(effect.id, &effect.name)
                        .map_or_else(|| effect.name.clone(), |e| name::mangle_effect_ref(&e))
                })
                .collect();
            name::FqTypeName::builtin(&name::mangle_fn_type(
                ft.is_mut,
                &params,
                &written_type_arg(&ft.return_type, resolutions).to_mangled(),
                matches!(ft.return_type, ast::Type::Function(_)),
                &with_clause,
            ))
        }
        _ => {
            let head = match head_site(ty).map(|site| resolutions.get(site)) {
                Some(Resolution::Def(def)) => name::FqTypeName::of_head(resolutions.defs(), def),
                Some(Resolution::Binder(_)) => name::FqTypeName::binder(&get_type_name_static(ty)),
                // A projection names no type until its base is one, and the
                // trait declaring the member is part of that name
                // (WEP-2026-08-12). A site that must know resolves it at its own
                // arguments rather than reading this spelling.
                Some(Resolution::Projection(_) | Resolution::Unresolved) | None => {
                    name::FqTypeName::builtin(&get_type_name_static(ty))
                }
            };
            match ty {
                ast::Type::Generic(generic) => head.with_args(nested(&generic.args)),
                // `ns::Pair<i32>` and `ns::Pair<bool>` are two instantiations
                // of one declaration. Dropping the arguments here mangled both
                // to the same segment, so the second `From` impl collided with
                // the first, and a structural comparison against the
                // argument's own type name matched neither.
                ast::Type::NamespacedGeneric(ns) => head.with_args(nested(&ns.args)),
                _ => head,
            }
        }
    }
}

/// The written form of `ty`, for a diagnostic saying what the programmer
/// wrote (WEP 2026-08-12 §9).
///
/// Renders the AST, so nothing reads it back into a declaration.
pub(super) fn written_type_source(ty: &ast::Type) -> String {
    let list = |args: &[ast::Type]| {
        args.iter()
            .map(written_type_source)
            .collect::<Vec<_>>()
            .join(", ")
    };
    match ty {
        ast::Type::Named(named) => named.name.clone(),
        ast::Type::Generic(g) => format!("{}<{}>", g.name, list(&g.args)),
        ast::Type::NamespacedGeneric(ns) => {
            format!("{}::{}<{}>", ns.namespace, ns.name, list(&ns.args))
        }
        ast::Type::Function(ft) => {
            let m = if ft.is_mut { " mut" } else { "" };
            format!(
                "fn{m}({}) -> {}",
                list(&ft.params),
                written_type_source(&ft.return_type)
            )
        }
        ast::Type::Tuple(elems) => format!("[{}]", list(elems)),
        ast::Type::Reference(inner) => format!("&{}", written_type_source(inner)),
        ast::Type::MutReference(inner) => format!("&mut {}", written_type_source(inner)),
        ast::Type::TypePackSpread(name, _) => format!("..{name}"),
        ast::Type::Infer(_) => "_".to_string(),
        ast::Type::Error(_) => "<error>".to_string(),
    }
}

pub(super) fn get_type_name_static(ty: &ast::Type) -> String {
    match ty {
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
    use crate::module_source::ModuleSourceInterner;

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
}
