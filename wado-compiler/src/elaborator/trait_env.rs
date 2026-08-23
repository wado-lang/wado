//! Global trait knowledge base: trait declarations, impl blocks, and blanket impls.
//!
//! `TraitEnv` is built once before resolution begins and is immutable thereafter.
//! It provides O(1) lookup of trait implementations by type name and trait name,
//! replacing linear scans across all modules.

use std::sync::Arc;

use crate::ast::{self, AstId, Item, Module, Type};
use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::kiln::InvocationIndex;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name;
use crate::tir::TypeTable;
use crate::token::Span;

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
                let source = crate::loader::resolve_use_decl_source(
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
    /// A shape no module declares — the tuple family (`[..T]`), a function
    /// type. It reaches no declaration by name, so no module qualifies it, and
    /// a definition and a lookup agree without either knowing where the other
    /// stood. A primitive is *not* here: `i32` and `()` are `internal type`
    /// declarations and resolve like any other name.
    Builtin(String),
}

impl ImplTargetKey {
    /// The key for a declaration already identified.
    ///
    /// The one place the builtin decision is made, mirroring
    /// [`name::FqTypeName::of_head`]: a shape every mangler spells bare drops
    /// its declaration, so a definition reached through a written head and a
    /// lookup reached through a resolved type land on the same key.
    pub(crate) fn of_decl(defs: &crate::defs::DefTable, def: DefId) -> Self {
        if name::is_builtin_shape_name(defs.name(def)) {
            return ImplTargetKey::Builtin(defs.name(def).to_string());
        }
        ImplTargetKey::Decl(def)
    }

    /// The key for a head that reaches no declaration — a shape every mangler
    /// spells bare, or a name that resolves to nothing.
    ///
    /// The one path that produces a key from a spelling, and it is reachable
    /// only where there is no declaration to produce one from.
    pub(crate) fn of_undeclared(module: &ModuleSource, name: &str) -> Self {
        if name::is_builtin_shape_name(name) {
            return ImplTargetKey::Builtin(name.to_string());
        }
        ImplTargetKey::Undeclared(module.clone(), name.to_string())
    }

    /// The receiver this target indexes under. Built from the same declaration
    /// `TypeTable::impl_receiver_key` reads off a resolved type, so a
    /// definition and a lookup agree by construction.
    pub(crate) fn receiver(&self, defs: &crate::defs::DefTable) -> name::Receiver {
        match self {
            ImplTargetKey::Decl(def) => name::Receiver::Type(name::FqTypeName::of_head(defs, *def)),
            ImplTargetKey::Undeclared(module, name) => {
                name::Receiver::Type(name::FqTypeName::shape(module, name))
            }
            // A type parameter names no declaration, so no module qualifies it.
            ImplTargetKey::TypeParam(_, name) => {
                name::Receiver::Type(name::FqTypeName::binder(name))
            }
            ImplTargetKey::Ref(kind) => name::Receiver::Ref(*kind),
            ImplTargetKey::Builtin(name) => name::Receiver::Type(name::FqTypeName::builtin(name)),
        }
    }

    pub(crate) fn type_name<'a>(&'a self, defs: &'a crate::defs::DefTable) -> Option<&'a str> {
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
    pub(crate) fn display_name<'a>(&'a self, defs: &'a crate::defs::DefTable) -> &'a str {
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
pub(crate) fn render_decl_name(defs: &crate::defs::DefTable, def: DefId) -> String {
    if defs.is_function_local(def) {
        return name::mangle_local_item_name(defs.name(def), defs.ast_id(def));
    }
    defs.name(def).to_string()
}

/// Target type → the trait impl blocks written for it. Built once from all
/// loaded modules so a method call costs a lookup rather than a scan.
pub(super) type TraitImplIndex = IndexMap<ImplTargetKey, Vec<(ModuleSource, AstId)>>;

type ReceiverImplIndex = IndexMap<name::Receiver, Vec<(ModuleSource, AstId)>>;

fn index_by_receiver(index: &TraitImplIndex, defs: &crate::defs::DefTable) -> ReceiverImplIndex {
    let mut out: ReceiverImplIndex = IndexMap::default();
    for (key, entries) in index {
        out.entry(key.receiver(defs))
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
    /// The trait this header implements, read from `Resolutions` rather than
    /// resolved a second time. This is what an impl index matches against, so a
    /// lookup compares declarations rather than spellings two modules can share
    /// (WEP 2026-08-12). `None` for an inherent block, and for a trait position
    /// whose site names no declaration.
    pub(super) trait_ref: Option<crate::defs::DefId>,
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
    pub(super) fn fq_trait(
        &self,
        resolutions: &crate::resolve::Resolutions,
    ) -> Option<name::FqTraitName> {
        let trait_type = self.trait_type.as_ref()?;
        match self.trait_key.as_ref()? {
            ImplTargetKey::Decl(def) => Some(
                name::FqTraitName::declared(resolutions.defs(), *def)
                    .with_args(written_type_args(trait_type, resolutions)),
            ),
            ImplTargetKey::TypeParam(_, name) => Some(name::FqTraitName::binder(name)),
            ImplTargetKey::Ref(_) | ImplTargetKey::Builtin(_) | ImplTargetKey::Undeclared(..) => {
                None
            }
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
    blanket_impls: &IndexMap<DefId, Vec<BlanketImpl>>,
    resolutions: &crate::resolve::Resolutions,
) -> IndexMap<(ModuleSource, AstId), Vec<(DefId, String)>> {
    let mut out: IndexMap<(ModuleSource, AstId), Vec<(DefId, String)>> = IndexMap::default();
    for blanket in blanket_impls.values().flatten() {
        let key = (blanket.module.clone(), blanket.ast_id);
        let Some(header) = impl_headers.get(&key) else {
            continue;
        };
        let pairs: Vec<(DefId, String)> = header
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
                Some((resolutions.declared(bound.id)?, assoc.name.clone()))
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
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    blanket_impls: &IndexMap<DefId, Vec<BlanketImpl>>,
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
                let Some(def) = resolutions.declared(bound.id) else {
                    return BlanketParamSource::Unresolved;
                };
                BlanketParamSource::Projection(def, assoc.name.clone())
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
    /// The member's declared rung; consulted only on an inherent impl.
    pub(super) visibility: ast::Visibility,
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
    /// The trait the bound's reference site names, `None` where it reaches no
    /// declaration.
    pub(crate) decl_ref: Option<crate::defs::DefId>,
    /// Associated types the bound pins to the receiver param itself (`Output`
    /// in `T: Mul<Output = T>`) — the only shape decidable against a candidate
    /// receiver; any other right-hand side is the instantiation's to answer.
    pub(crate) pinned_to_receiver: Vec<String>,
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

/// Every `trait` declaration in the program. Membership is the question — is
/// this declaration a trait? — and the declaration is its own answer, so there
/// is nothing to store beside it.
pub(super) type TraitDeclIndex = IndexSet<DefId>;

/// A supertrait paired with the declaration it resolved to. The bound keeps the
/// declaring module's spelling, which need not name the same trait elsewhere.
///
/// `writer` is the trait whose declaration listed it: `trait Foo<A>:
/// Bar<Item = A>` binds `Item` to `Foo`'s `A`, not an asking frame's.
#[derive(Clone, Debug)]
pub(super) struct InheritedBound {
    pub(super) bound: ast::TraitBound,
    pub(super) decl: DefId,
    pub(super) writer: DefId,
}

/// Pre-built index: trait declaration → the transitive closure of its
/// supertraits, deduplicated by declaration and excluding the trait itself. A
/// declared bound `T: Sub` expands through this so `T: Ord` alone carries
/// `Eq`.
pub(super) type SupertraitClosureIndex = IndexMap<DefId, Vec<InheritedBound>>;

/// Every `interface` declaration. Effects are first-class citizens distinct
/// from traits and have their own impl form (`impl Effect for Type`)
/// interpreted as installable handlers, so the elaborator and dispatch
/// synthesis need to distinguish them quickly.
pub(super) type EffectDeclIndex = IndexSet<DefId>;

/// Every `resource` declaration. Resources participate in
/// `with R => h do` / `impl R for Type` exactly like effects (see WEP
/// 2026-04-11): both kinds of declaration carry a list of operations that
/// user handler implementations satisfy and that the dispatch-synthesis
/// pass routes through wrappers. Indexed separately from effects so the
/// elaborator can keep diagnostics ("not an effect", "not a resource")
/// truthful and so the dispatch synthesis can know not to declare the
/// resource on its wrapper's `effects` list (resources are not effects).
pub(super) type ResourceDeclIndex = IndexSet<DefId>;

/// Pre-built index of static methods (no `self` parameter) from impl blocks.
/// Key: canonical receiver [`DefId`] → list of
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
pub(super) type StaticMethodIndex = IndexMap<ImplTargetKey, Vec<StaticMethodEntry>>;

/// Pre-built index of static methods from resource declarations.
/// Key: canonical receiver [`DefId`] → `[(method_name, ModuleSource,
/// item_ast_id, method_index)]`. Same disambiguation rationale as
/// [`StaticMethodIndex`].
pub(super) type ResourceStaticMethodIndex =
    IndexMap<ImplTargetKey, Vec<(String, ModuleSource, AstId, usize)>>;

/// `(type_name, trait_name)` → modules holding that `impl` block. Keyed by bare
/// names rather than [`DefId`]: the multi-value `Vec` plus the caller's
/// `type_module` hint already routes two modules' same-named receivers apart.
/// Value blanket impls apply structurally, with no concrete receiver name, and
/// live in `blanket_impls` instead.
pub(crate) type TraitImplModuleIndex = IndexMap<(String, String), Vec<ModuleSource>>;

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

/// Where every non-blanket `impl` block lives, in both receiver namespaces, read
/// off the headers' resolved identities rather than the heads they wrote.
/// `concrete_only` keeps just the parameterless blocks: the monomorphizer sends
/// a substituted call to a concrete impl's own module, while a generic impl's
/// instance is materialised in the receiver type's.
fn index_impl_modules(
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    resolutions: &crate::resolve::Resolutions,
    concrete_only: bool,
) -> ImplModuleIndex {
    let defs = resolutions.defs();
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
        let Some(fq_trait) = header.fq_trait(resolutions) else {
            continue;
        };
        out.record(
            &header.target.receiver(defs),
            fq_trait.base_name(),
            &header.module,
        );
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
    /// Trait name → trait declaration location.
    pub(super) decl_index: TraitDeclIndex,
    /// Every declaration in the program. Held here so a query keyed by an
    /// identity can render one for a diagnostic without every caller threading
    /// the table.
    pub(crate) defs: std::sync::Arc<crate::defs::DefTable>,
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
    /// spelling the blanket wrote (WEP 2026-08-12).
    pub(super) blanket_pack_assocs: IndexMap<(ModuleSource, AstId), Vec<(DefId, String)>>,
    /// Per blanket impl, what determines each of its parameters, in
    /// declaration order. Resolved once at build time from each bound's own
    /// reference site.
    pub(super) blanket_param_sources: IndexMap<(ModuleSource, AstId), Vec<BlanketParamSource>>,
    /// Digested headers for every `trait` declaration, keyed by
    /// `(ModuleSource, AstId)`. Lets method-lookup queries read trait
    /// method signatures without re-fetching the trait AST. See
    /// [`TraitDeclHeader`].
    pub(super) trait_decl_headers: IndexMap<DefId, TraitDeclHeader>,
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
    /// Declared name → every declaration written under it, in build order.
    /// The frame derivation's second tier reads this on every name that is not
    /// an import, so it is keyed by name rather than scanned: the sets it
    /// unions are whole-program, and every prelude spelling — `i32`, `String`,
    /// `List` — would otherwise walk all of them before the prelude answered.
    decls_by_name: IndexMap<String, Vec<DefId>>,
    /// Per-module namespace-import aliases, pre-computed once, so a query
    /// standing in a foreign module's perspective reads them instead of
    /// re-walking its `use` declarations. See [`namespace_imports_of`].
    pub(super) module_namespace_imports: IndexMap<ModuleSource, NamespaceImports>,
    /// Associated-type name → the declaring trait's bounds for it, first
    /// declaration wins (matching the previous whole-program scan order).
    /// Consumed by `find_assoc_type_bounds` without an AST scan.
    pub(super) assoc_type_bound_index: IndexMap<String, Vec<ast::TraitBound>>,
    /// Blanket impls by the trait they implement, in registration order. The
    /// single classification source for blanket dispatch (module, receiver
    /// kind, param, bounds), and where the monomorphizer finds the home module
    /// of a generic dispatch the receiver wrote no impl for.
    ///
    /// Keyed by declaration: a name-keyed bucket merged two modules'
    /// same-named traits, so one module's blanket answered the other's bound
    /// (WEP 2026-08-12).
    pub(super) blanket_impls: IndexMap<DefId, Vec<BlanketImpl>>,
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
        resolutions: &crate::resolve::Resolutions,
    ) -> (Arc<Self>, Vec<(ModuleSource, TypeError)>) {
        let mut module_namespace_imports: IndexMap<ModuleSource, NamespaceImports> =
            IndexMap::default();
        for (module_source, module) in modules {
            module_namespace_imports.insert(
                module_source.clone(),
                namespace_imports_of(interner, module, module_source, entry_module, invocations),
            );
        }
        let mut impl_index: TraitImplIndex = IndexMap::default();
        let mut all_impl_index: TraitImplIndex = IndexMap::default();
        let mut decl_index: TraitDeclIndex = IndexSet::default();
        let mut effect_decl_index: EffectDeclIndex = IndexSet::default();
        let mut assoc_type_bound_index: IndexMap<String, Vec<ast::TraitBound>> =
            IndexMap::default();
        let mut resource_decl_index: ResourceDeclIndex = IndexSet::default();
        let mut blanket_impls: IndexMap<DefId, Vec<BlanketImpl>> = IndexMap::default();
        let mut impl_headers: IndexMap<(ModuleSource, AstId), ImplHeader> = IndexMap::default();
        let mut trait_decl_headers: IndexMap<DefId, TraitDeclHeader> = IndexMap::default();
        let mut function_type_params: IndexMap<(ModuleSource, String), Vec<ast::GenericParam>> =
            IndexMap::default();
        let mut struct_like_decl_modules: IndexMap<String, Vec<DefId>> = IndexMap::default();
        let mut newtype_decl_modules: IndexMap<String, Vec<DefId>> = IndexMap::default();
        // Every type declaration, for the orphan rule's "does this package own
        // it?" check. A declaration, so a user type shadowing a stdlib name
        // cannot vouch for the stdlib type it shadows.
        let mut type_decl_index: IndexSet<DefId> = IndexSet::default();

        let mut static_method_index: StaticMethodIndex = IndexMap::default();
        let mut resource_static_method_index: ResourceStaticMethodIndex = IndexMap::default();

        // Pass 1: walk every module's items to populate the
        // declaration-side indices (trait / effect / resource / type
        // decls). We need these populated *before* impl blocks are
        // canonicalised in pass 2, because the build-time canonical-key
        // helper falls back to scanning the decl indices when the
        // per-module symbol table misses (typical for prelude-implicit
        // names that no `use` declaration explicitly threads).
        let defs = resolutions.defs();
        for (module_source, module) in modules {
            for item in &module.items {
                match item {
                    Item::Trait(trait_decl) => {
                        // Keyed by the declaration, so two modules declaring a
                        // same-named trait cannot share an entry. The previous
                        // bare-name key first-wrote-wins and silently routed
                        // both declarations to the same one.
                        if let Some(def) = defs.of_ast_id(trait_decl.id) {
                            decl_index.insert(def);
                        }
                        for assoc in &trait_decl.associated_types {
                            assoc_type_bound_index
                                .entry(assoc.name.clone())
                                .or_insert_with(|| assoc.bounds.clone());
                        }
                    }
                    Item::Interface(effect_decl) => {
                        if let Some(def) = defs.of_ast_id(effect_decl.id) {
                            effect_decl_index.insert(def);
                        }
                    }
                    Item::Resource(resource) => {
                        let Some(resource_key) = defs.of_ast_id(resource.id) else {
                            continue;
                        };
                        resource_decl_index.insert(resource_key);
                        // Index static methods from resource declarations.
                        // The resource declaration itself is the receiver.
                        for (method_idx, method) in resource.methods.iter().enumerate() {
                            let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == resource.name)
                            });
                            if !has_self {
                                resource_static_method_index
                                    .entry(ImplTargetKey::Decl(resource_key))
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
                        if let Some(def) = defs.of_ast_id(s.id) {
                            type_decl_index.insert(def);
                        }
                    }
                    Item::Variant(v) => {
                        if let Some(def) = defs.of_ast_id(v.id) {
                            type_decl_index.insert(def);
                        }
                    }
                    Item::Enum(e) => {
                        if let Some(def) = defs.of_ast_id(e.id) {
                            type_decl_index.insert(def);
                        }
                    }
                    Item::Flags(f) => {
                        if let Some(def) = defs.of_ast_id(f.id) {
                            type_decl_index.insert(def);
                        }
                    }
                    Item::Newtype(n) => {
                        if let Some(def) = defs.of_ast_id(n.id) {
                            type_decl_index.insert(def);
                        }
                    }
                    Item::BuiltinTypeDecl(d) => {
                        if let Some(def) = defs.of_ast_id(d.id) {
                            type_decl_index.insert(def);
                        }
                    }
                    Item::TupleTypeDecl(t) => {
                        if let Some(def) = defs.of_ast_id(t.id) {
                            type_decl_index.insert(def);
                        }
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
                // Digest the per-item facts `lookup_function_type_params` and
                // `decls_by_name` are built from, so neither needs to re-scan
                // `loaded_modules`. (Non-impl items fall through to the
                // `Item::Impl` guard below and `continue`.)
                match item {
                    Item::Function(f) => {
                        function_type_params.insert(
                            (module_source.clone(), f.name.clone()),
                            f.type_params.clone(),
                        );
                    }
                    Item::Struct(s) => {
                        if let Some(def) = defs.of_ast_id(s.id) {
                            struct_like_decl_modules
                                .entry(s.name.clone())
                                .or_default()
                                .push(def);
                        }
                    }
                    Item::Resource(r) => {
                        if let Some(def) = defs.of_ast_id(r.id) {
                            struct_like_decl_modules
                                .entry(r.name.clone())
                                .or_default()
                                .push(def);
                        }
                    }
                    Item::Variant(v) => {
                        if let Some(def) = defs.of_ast_id(v.id) {
                            struct_like_decl_modules
                                .entry(v.name.clone())
                                .or_default()
                                .push(def);
                        }
                    }
                    Item::Enum(e) => {
                        if let Some(def) = defs.of_ast_id(e.id) {
                            struct_like_decl_modules
                                .entry(e.name.clone())
                                .or_default()
                                .push(def);
                        }
                    }
                    Item::BuiltinTypeDecl(d) => {
                        if let Some(def) = defs.of_ast_id(d.id) {
                            struct_like_decl_modules
                                .entry(d.name.clone())
                                .or_default()
                                .push(def);
                        }
                    }
                    Item::Newtype(n) => {
                        if let Some(def) = defs.of_ast_id(n.id) {
                            newtype_decl_modules
                                .entry(n.name.clone())
                                .or_default()
                                .push(def);
                        }
                    }
                    _ => {}
                }
                if let Item::Trait(trait_decl) = item {
                    let Some(trait_def) = defs.of_ast_id(trait_decl.id) else {
                        continue;
                    };
                    trait_decl_headers.insert(
                        trait_def,
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
                                    visibility: m.visibility,
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
                let type_key = impl_target_key_at(&impl_block.ty, module_source, resolutions);
                let trait_ref: Option<crate::defs::DefId> = impl_block
                    .trait_type
                    .as_ref()
                    .and_then(crate::resolve::head_site)
                    .and_then(|site| resolutions.declared(site));
                // Implementing a trait is naming it, so the header's own
                // site answers and a position reaching nothing is an error —
                // never another module's same-named trait.
                let trait_key = impl_block.trait_type.as_ref().map(|trait_type| {
                    trait_ref.map_or_else(
                        || impl_target_key_at(trait_type, module_source, resolutions),
                        ImplTargetKey::Decl,
                    )
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
                                visibility: m.visibility,
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
                                        name: b.name.clone(),
                                        decl_ref: resolutions.declared(b.id),
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
                                    ast_id: impl_block.id,
                                    receiver,
                                    param,
                                    bounds,
                                });
                        }
                    }
                    impl_index
                        .entry(type_key.clone())
                        .or_default()
                        .push((module_source.clone(), impl_block.id));
                    // Static methods on trait impl blocks (no `self`
                    // parameter) join the same canonical bucket as
                    // inherent statics. `f64::from_bits` and friends in
                    // `core:prelude/int128.wado` flow through this path.
                    let recv_key = type_key.clone();
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
                    let recv_key = type_key.clone();
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
            let key = resolutions.declared(bound.id)?;
            decl_index.contains(&key).then_some(key)
        };
        let trait_impl_modules = index_impl_modules(&impl_headers, resolutions, false);
        let concrete_trait_impl_modules = index_impl_modules(&impl_headers, resolutions, true);
        let decls_by_name = index_decls_by_name(
            defs,
            [
                &type_decl_index,
                &decl_index,
                &effect_decl_index,
                &resource_decl_index,
            ],
            [&struct_like_decl_modules, &newtype_decl_modules],
        );

        violations.extend(check_variadic_impl_overlap(defs, &impl_headers));
        violations.extend(check_inherent_impl_collisions(
            defs,
            &impl_headers,
            resolutions,
        ));

        let (supertrait_closures, cycles) =
            build_supertrait_closures(defs, &trait_decl_headers, &resolve_trait);
        violations.extend(cycles);

        (
            Arc::new(Self {
                by_receiver: index_by_receiver(&impl_index, defs),
                all_by_receiver: index_by_receiver(&all_impl_index, defs),
                impl_index,
                all_impl_index,
                decl_index,
                defs: resolutions.defs().clone(),
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
                decls_by_name,
                module_namespace_imports,
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

    /// The pre-computed namespace aliases for `module`. Empty for a module
    /// with none.
    pub(super) fn namespace_imports(&self, module: &ModuleSource) -> NamespaceImports {
        self.module_namespace_imports
            .get(module)
            .cloned()
            .unwrap_or_default()
    }

    /// The transitive supertraits of the trait `key` names, deduplicated by
    /// declaration and excluding the trait itself. Empty for a trait with no
    /// supertrait clause, and for a name that declares no trait.
    pub(super) fn supertrait_closure(&self, key: &DefId) -> &[InheritedBound] {
        self.supertrait_closures.get(key).map_or_else(
            || self.supertrait_closure_named(self.defs.name(*key)),
            Vec::as_slice,
        )
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

    /// Whether `key` names a trait declaration.
    pub(crate) fn declares_trait(&self, key: &DefId) -> bool {
        self.decl_index.contains(key)
    }

    /// Every declaration written under `name`, whichever module declares it.
    ///
    /// The frame derivation's raw material, and not a scope: it holds what
    /// modules *declare*, never what they import, so no alias can steer it, and
    /// it takes no vantage — the caller filters for the module it means.
    pub(crate) fn decls_named<'n>(&'n self, name: &str) -> impl Iterator<Item = DefId> + 'n {
        self.decls_by_name.get(name).into_iter().flatten().copied()
    }

    /// The trait `header` implements, named as a bound can reach it. See
    /// [`args_without_declared_defaults`] for why an argument may drop out.
    pub(super) fn fq_trait_of_impl(
        &self,
        header: &ImplHeader,
        resolutions: &crate::resolve::Resolutions,
    ) -> Option<name::FqTraitName> {
        let fq = header.fq_trait(resolutions)?;
        let trait_type = header.trait_type.as_ref()?;
        Some(self.fq_trait_named_by_impl(fq, trait_type, &header.ty, resolutions))
    }

    /// [`Self::fq_trait_of_impl`] for a caller holding the written trait
    /// reference and the impl's target rather than a built header.
    pub(super) fn fq_trait_named_by_impl(
        &self,
        fq: name::FqTraitName,
        trait_type: &ast::Type,
        target: &ast::Type,
        resolutions: &crate::resolve::Resolutions,
    ) -> name::FqTraitName {
        let Some(params) = fq
            .canonical()
            .and_then(|decl| self.trait_decl_headers.get(&decl))
            .map(|header| &header.type_params)
        else {
            return fq;
        };
        let args = args_without_declared_defaults(
            fq.args().to_vec(),
            trait_type,
            target,
            params,
            resolutions,
        );
        fq.with_args(args)
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
        trait_: crate::defs::DefId,
    ) -> bool {
        self.entries_by_receiver(receiver)
            .any(|entry| self.methodful_header_matches(entry, trait_))
    }

    /// `key` itself when it declares a trait, else `None` — the question the
    /// callers actually ask, phrased as the identity they then compare.
    pub(crate) fn trait_def(&self, key: &DefId) -> Option<crate::defs::DefId> {
        self.decl_index.contains(key).then_some(*key)
    }

    /// The trait an [`crate::name::FqTraitName`] names, when it names a trait
    /// declaration.
    pub(crate) fn trait_def_of_fq(&self, fq: &name::FqTraitName) -> Option<crate::defs::DefId> {
        self.trait_def(&fq.canonical()?)
    }

    /// [`Self::has_any_methodful_impl_by_receiver`] narrowed to the impls
    /// `module_source` itself writes.
    pub(crate) fn has_methodful_impl_by_receiver(
        &self,
        receiver: &name::Receiver,
        trait_: crate::defs::DefId,
        module_source: &ModuleSource,
    ) -> bool {
        self.entries_by_receiver(receiver)
            .any(|entry| entry.0 == *module_source && self.methodful_header_matches(entry, trait_))
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
        trait_: crate::defs::DefId,
    ) -> bool {
        self.impl_headers
            .get(entry)
            .is_some_and(|header| !header.methods.is_empty() && header.trait_ref == Some(trait_))
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
    pub(crate) fn pack_assocs_of_blanket(&self, blanket: &BlanketImpl) -> Vec<(DefId, String)> {
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
            .iter()
            .find(|def| self.defs.name(**def) == name && !is_user_local(self.defs.module(**def)))
            .map(|def| ImplTargetKey::Decl(*def))
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
    resolutions: &crate::resolve::Resolutions,
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
    resolutions: &crate::resolve::Resolutions,
) -> Option<ImplTargetKey> {
    // A reference target buckets by kind alone: the table resolves `&List<T>`
    // to `List`, which is the referent, not the bucket.
    if let Some(kind) = name::RefKind::from_ast(ty) {
        return Some(ImplTargetKey::Ref(kind));
    }
    let site = crate::resolve::head_site(ty)?;
    match resolutions.get(site) {
        // The impl's own binder, which shadows any declaration of that name —
        // `impl<T> Trait for T` written where a `struct T` exists stays a
        // blanket.
        crate::resolve::Resolution::Binder(_) => Some(ImplTargetKey::TypeParam(
            module_source.clone(),
            get_type_name_static(ty),
        )),
        crate::resolve::Resolution::Def(def) => {
            Some(ImplTargetKey::of_decl(resolutions.defs(), def))
        }
        crate::resolve::Resolution::Unresolved => None,
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
type ResolveTrait<'a> = &'a dyn Fn(&ast::TraitBound) -> Option<DefId>;

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
    defs: &crate::defs::DefTable,
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
    defs: &crate::defs::DefTable,
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
            // Blame the declaration, not every implementor of it.
            cycles.push((
                defs.module(loc).clone(),
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
            report_supertrait_cycle(defs, pos, stack, headers, reported, cycles);
            continue;
        }
        push_unique_inherited(
            &mut closure,
            &InheritedBound {
                bound: direct.clone(),
                decl: super_loc,
                writer: loc,
            },
        );
        for inherited in expand_supertraits(
            defs, super_loc, headers, resolve, closures, stack, reported, cycles,
        ) {
            push_unique_inherited(&mut closure, &inherited);
        }
    }
    stack.pop();

    closures.insert(loc, closure.clone());
    closure
}

/// Re-key the closures by bare trait name, dropping any name more than one
/// module declares — an ambiguous name must not silently pick a closure.
fn index_closures_by_name(
    headers: &IndexMap<DefId, TraitDeclHeader>,
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
    defs: &crate::defs::DefTable,
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

/// Coherence Rule 2 (WEP 2026-03-14 §5): two variadic impls of one trait accept
/// the same tuples, and a pack's bounds resolve only at monomorphization, so
/// nothing separates them at selection — reject the later one where it is
/// written. Grouping is by trait *declaration*, so two modules may each keep
/// their own. The same walk refuses a target the compiler cannot implement.
fn check_variadic_impl_overlap(
    defs: &crate::defs::DefTable,
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
            trait_name: trait_key.display_name(defs).to_string(),
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
        ast::Type::NamespacedGeneric(ns) => ns
            .args
            .iter()
            .any(|a| target_mentions_impl_param(a, params)),
        ast::Type::Function(_) | ast::Type::Infer(_) | ast::Type::Error(_) => false,
    }
}

/// An inherent `impl Box_<i32>` and an inherent `impl<T> Box_<T>` defining the
/// same method both own the name `Box_<i32>::a`. A trait impl would force one
/// signature on both, letting coherence Rule 1 pick the specific one; an
/// inherent impl carries no such contract, so a generic caller type-checked
/// against the general method would link to a differently-typed function.
/// Rejected, as in Rust. Keyed by the resolved [`ImplTargetKey`], never the
/// written head — two modules' `Box_` are two types, and a spelling cannot say so.
fn check_inherent_impl_collisions(
    defs: &crate::defs::DefTable,
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    resolutions: &crate::resolve::Resolutions,
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

    // Two inherent impls minting one function name are one definition
    // downstream, which monomorphization asserts away with a panic. Keyed on
    // what the definition side mints, since the target's *arguments* answer a
    // different question and discard the pointee of a reference target.
    let mut minted: IndexSet<(String, &str)> = IndexSet::default();
    for header in &instantiations {
        let peeled = match &header.ty {
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => inner.as_ref(),
            other => other,
        };
        let receiver = written_type_arg(peeled, resolutions);
        for method in &header.methods {
            if !minted.insert((receiver.to_mangled(), method.name.as_str())) {
                violations.push((
                    header.module.clone(),
                    TypeError::DuplicateInherentMethod {
                        // What this impl wrote: the minted head is one
                        // string for both, so it names neither.
                        self_type_name: written_type_source(&header.ty),
                        method_name: method.name.clone(),
                        span: method.span,
                    },
                ));
            }
        }
    }

    for header in instantiations {
        let Some(generic_methods) = generic_methods_by_target.get(&header.target) else {
            continue;
        };
        for method in &header.methods {
            if generic_methods.contains(method.name.as_str()) {
                violations.push((
                    header.module.clone(),
                    TypeError::DuplicateInherentMethod {
                        self_type_name: header.target.display_name(defs).to_string(),
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
    defs: &crate::defs::DefTable,
    impl_headers: &IndexMap<(ModuleSource, AstId), ImplHeader>,
    decl_index: &TraitDeclIndex,
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

        let Some(trait_key) = &header.trait_key else {
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

/// Invert the declaration indexes into declared name → declarations, name-keyed
/// maps first so source order is kept, and each declaration landing once — a
/// duplicate would make a caller taking the unique answer see two.
fn index_decls_by_name(
    defs: &crate::defs::DefTable,
    sets: [&IndexSet<DefId>; 4],
    maps: [&IndexMap<String, Vec<DefId>>; 2],
) -> IndexMap<String, Vec<DefId>> {
    let mut out: IndexMap<String, Vec<DefId>> = IndexMap::default();
    let mut push = |def: DefId| {
        let entry = out.entry(defs.name(def).to_string()).or_default();
        if !entry.contains(&def) {
            entry.push(def);
        }
    };
    for map in maps {
        for def in map.values().flatten() {
            push(*def);
        }
    }
    for set in sets {
        for def in set {
            push(*def);
        }
    }
    out
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
    resolutions: &crate::resolve::Resolutions,
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
    trait_type: &ast::Type,
    target: &ast::Type,
    params: &[ast::GenericParam],
    resolutions: &crate::resolve::Resolutions,
) -> Vec<name::FqTypeName> {
    let mut kept = written;
    kept.truncate(non_default_arg_count(
        trait_type,
        target,
        params,
        resolutions,
    ));
    kept
}

/// Whether a bare bound on the trait selects this header. A bound writes no
/// arguments, so it asks for the declared default where a position has one
/// (`T: Mul` is `Mul<Self>`) and nothing where it has none (`T: Pick`).
pub(super) fn header_answers_bare_bound(
    trait_type: &ast::Type,
    target: &ast::Type,
    params: &[ast::GenericParam],
    resolutions: &crate::resolve::Resolutions,
) -> bool {
    let ast_args = written_arg_nodes(trait_type);
    ast_args.iter().enumerate().all(|(i, arg)| {
        params
            .get(i)
            .and_then(|p| p.default.as_ref())
            .is_none_or(|default| restates_default(arg, default, target, resolutions))
    })
}

/// Whether a written trait argument says exactly what the declared default
/// does, `Self` meaning the impl's target.
fn restates_default(
    arg: &ast::Type,
    default: &ast::Type,
    target: &ast::Type,
    resolutions: &crate::resolve::Resolutions,
) -> bool {
    match default {
        ast::Type::Named(named) if named.name == "Self" => {
            matches!(arg, ast::Type::Named(a) if a.name == "Self")
                || written_type_arg(arg, resolutions) == written_type_arg(target, resolutions)
        }
        _ => written_type_arg(arg, resolutions) == written_type_arg(default, resolutions),
    }
}

/// How many of `trait_type`'s written arguments say something its declared
/// defaults do not. One rule behind both an impl's name and the identity its
/// associated types register under, so the two cannot disagree.
pub(super) fn non_default_arg_count(
    trait_type: &ast::Type,
    target: &ast::Type,
    params: &[ast::GenericParam],
    resolutions: &crate::resolve::Resolutions,
) -> usize {
    let ast_args = written_arg_nodes(trait_type);
    let mut kept = ast_args.len();
    while let Some(last) = kept.checked_sub(1) {
        let (Some(arg), Some(param)) = (ast_args.get(last), params.get(last)) else {
            break;
        };
        let Some(default) = param.default.as_ref() else {
            break;
        };
        if !restates_default(arg, default, target, resolutions) {
            break;
        }
        kept = last;
    }
    kept
}

/// One written type argument as the identity it names.
///
/// A name that reaches no declaration keeps its spelling — there is no identity
/// to hold, and [`name::TypeHead::Builtin`] is the case that says so.
pub(super) fn written_type_arg(
    ty: &ast::Type,
    resolutions: &crate::resolve::Resolutions,
) -> name::FqTypeName {
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
        ast::Type::Tuple(elems) if elems.is_empty() => {
            name::FqTypeName::builtin(TypeTable::UNIT_TYPE_NAME)
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
            // The written spelling: an effect has no reference site, so there
            // is no identity to ask for. The resolved side qualifies a concrete
            // effect by module, so the two agree on a binder and not on one.
            let mut with_clause: Vec<String> = ft.effects.clone();
            for entry in &ft.stores {
                with_clause.push(match entry {
                    ast::StoresEntry::Index(index) => name::mangle_stores_member(*index),
                    // A name has no parameter position here, and inventing one
                    // would name a parameter the type never mentions.
                    ast::StoresEntry::Name(name) => format!("stores[{name}]"),
                });
            }
            name::FqTypeName::builtin(&name::mangle_fn_type(
                ft.is_mut,
                &params,
                &written_type_arg(&ft.return_type, resolutions).to_mangled(),
                matches!(ft.return_type, ast::Type::Function(_)),
                &with_clause,
            ))
        }
        _ => {
            let head = match crate::resolve::head_site(ty).map(|site| resolutions.get(site)) {
                Some(crate::resolve::Resolution::Def(def)) => {
                    name::FqTypeName::of_head(resolutions.defs(), def)
                }
                Some(crate::resolve::Resolution::Binder(_)) => {
                    name::FqTypeName::binder(&get_type_name_static(ty))
                }
                Some(crate::resolve::Resolution::Unresolved) | None => {
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
fn written_type_source(ty: &ast::Type) -> String {
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
