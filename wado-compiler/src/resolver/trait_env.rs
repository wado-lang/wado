//! Global trait knowledge base: trait declarations, impl blocks, and blanket impls.
//!
//! `TraitEnv` is built once before resolution begins and is immutable thereafter.
//! It provides O(1) lookup of trait implementations by type name and trait name,
//! replacing linear scans across all modules.

use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::ast::{self, Item, Module, Type};
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{TypeId, TypeTable};

use super::Resolver;
use super::types::TypeError;
use crate::symbol::SymbolTable;

/// Pick the right `ModuleSource` out of the AST + synthesised candidate
/// lists. If `prefer` is supplied and present in either list, return that
/// one; otherwise fall back to the first AST entry, then the first
/// synthesised entry. The union prevents an AST-layer entry from masking
/// a same-keyed synthesised entry (e.g. core's variadic
/// `impl<..T> Inspect for [..T]` registers `("Tuple", "Inspect")` at the
/// AST layer; a user `struct Tuple` whose auto-derived `Tuple^Inspect`
/// lives in the synthesised layer must still be reachable when its module
/// matches the hint).
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
/// these kinds — the resolver canonicalises a use-site bare name through
/// the symbol table ([`crate::resolver::Resolver::canonical_decl_key`])
/// before consulting the index.
pub(crate) type DeclKey = (ModuleSource, String);

/// Pre-built index: type name → list of (`ModuleSource`, item index) for trait impl blocks.
/// Built once from all loaded modules to avoid O(all items) scans per method call.
///
/// Keyed by the bare receiver type name on purpose: the lookup iterates the
/// candidate `Vec` and disambiguates each entry via its `(ModuleSource,
/// item_idx)` payload plus the resolver's per-call type-id comparison, so
/// two `struct Widget` declarations in different modules share one bucket
/// without ambiguity.
pub(super) type TraitImplIndex = IndexMap<String, Vec<(ModuleSource, usize)>>;

/// Pre-built index: `(declaring module, trait name)` → (`ModuleSource`, item index)
/// for trait declarations.
pub(super) type TraitDeclIndex = IndexMap<DeclKey, (ModuleSource, usize)>;

/// Pre-built index: `(declaring module, effect name)` → (`ModuleSource`, item
/// index) for effect declarations. Effects are first-class citizens distinct
/// from traits and have their own impl form (`impl Effect for Type`)
/// interpreted as installable handlers, so the resolver and dispatch
/// synthesis need to distinguish them quickly.
pub(super) type EffectDeclIndex = IndexMap<DeclKey, (ModuleSource, usize)>;

/// Pre-built index: `(declaring module, resource name)` → (`ModuleSource`,
/// item index) for resource declarations. Resources participate in
/// `with R => h do` / `impl R for Type` exactly like effects (see WEP
/// 2026-04-11): both kinds of declaration carry a list of operations that
/// user handler implementations satisfy and that the dispatch-synthesis
/// pass routes through wrappers. Indexed separately from effects so the
/// resolver can keep diagnostics ("not an effect", "not a resource")
/// truthful and so the dispatch synthesis can know not to declare the
/// resource on its wrapper's `effects` list (resources are not effects).
pub(super) type ResourceDeclIndex = IndexMap<DeclKey, (ModuleSource, usize)>;

/// Pre-built list of blanket trait impls: `impl<T: Trait> OtherTrait for T`.
/// These are impl blocks where the impl type is a free type parameter with trait bounds.
/// Stored separately because they can't be indexed by concrete type name.
pub(super) type BlanketTraitImplIndex = Vec<(ModuleSource, usize)>;

/// Pre-built index of static methods (no `self` parameter) from impl blocks.
/// Key: canonical receiver [`DeclKey`] → list of
/// `(method_name, impl_module_source, item_index, method_index)`.
/// Enables O(1) lookup of static methods instead of scanning all modules.
///
/// Keyed by canonical declaration key (rather than bare type name) so that
/// `impl Counter { fn make(...) }` declared in two different modules with
/// same-named `struct Counter` produces two distinct buckets — the lookup
/// at `CounterA::make(...)` resolves through the call-site's import context
/// and dispatches to the right module's impl.
pub(super) type StaticMethodIndex = IndexMap<DeclKey, Vec<(String, ModuleSource, usize, usize)>>;

/// Pre-built index of static methods from resource declarations.
/// Key: canonical receiver [`DeclKey`] → `[(method_name, ModuleSource,
/// item_index, method_index)]`. Same disambiguation rationale as
/// [`StaticMethodIndex`].
pub(super) type ResourceStaticMethodIndex =
    IndexMap<DeclKey, Vec<(String, ModuleSource, usize, usize)>>;

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
/// Blanket impls (`impl<T: Trait> Trait for T`) are still represented by
/// [`BlanketTraitImplIndex`]; they are excluded from this map because they
/// apply structurally and don't have a concrete receiver type name.
pub(crate) type TraitImplModuleIndex = IndexMap<(String, String), Vec<ModuleSource>>;

/// Immutable global knowledge base for trait resolution.
///
/// Contains pre-built indices for fast lookup of trait implementations,
/// trait declarations, and blanket impls. Built once before resolution
/// begins and shared (via `Arc`) across all module resolvers.
/// `TraitEnv` is *intentionally* not `Clone`. After `build()` returns, the
/// only legitimate way to mutate the env is `extend_with_synthesised`,
/// which moves out of an `Arc` whose strong count must be 1. Forbidding
/// clones at the type level surfaces accidental `Arc` sharing as a
/// compile error rather than a silent deep-clone of every index.
#[derive(Debug)]
pub struct TraitEnv {
    /// Type name → impl blocks that implement traits for that type.
    pub(super) impl_index: TraitImplIndex,
    /// Trait name → trait declaration location.
    pub(super) decl_index: TraitDeclIndex,
    /// Effect name → effect declaration location.
    pub(super) effect_decl_index: EffectDeclIndex,
    /// Resource name → resource declaration location. Used alongside
    /// `effect_decl_index` to recognise handler-installable kinds in `with`
    /// clauses and `impl R for T` blocks.
    pub(super) resource_decl_index: ResourceDeclIndex,
    /// Blanket impls (`impl<T: Bound> Trait for T`), checked as fallback.
    pub(super) blanket_impl_index: BlanketTraitImplIndex,
    /// `trait_name` → modules that host a blanket impl of that trait
    /// (`impl<T: Bound> Trait for T`). Used by the monomorphizer to find
    /// the home module of a generic dispatch when the receiver type
    /// itself has no dedicated `impl Trait for Type` block — the blanket
    /// provides the body, and the body lives in the blanket's module.
    ///
    /// Keyed by bare trait name (see [`TraitImplModuleIndex`] for the
    /// rationale): canonical-key disambiguation would require build-time
    /// resolution of the trait reference, which isn't plumbed through
    /// `TraitEnv::build`. The multi-value `Vec<ModuleSource>` plus the
    /// `type_module` hint at the call site handles the common case.
    pub(crate) blanket_trait_impl_modules: IndexMap<String, Vec<ModuleSource>>,
    /// `type_name` → `[(method_name, ModuleSource, item_idx, method_idx)]` for static methods.
    pub(super) static_method_index: StaticMethodIndex,
    /// `type_name` → `[(method_name, ModuleSource, item_idx, method_idx)]` for resource static methods.
    pub(super) resource_static_method_index: ResourceStaticMethodIndex,
    /// `(type_name, trait_name)` → `ModuleSource` of the non-blanket impl.
    /// Consumed by template synthesis to route trait-method calls to the
    /// module that actually defines the impl, regardless of where the
    /// receiver type is declared (e.g. `impl Display for String` lives in
    /// `core:prelude/format`, not the module that declares `String`).
    /// Includes both fully concrete impls (`impl Display for String`) and
    /// generic impls (`impl<T: Inspect> Inspect for Array<T>`).
    pub(crate) trait_impl_modules: TraitImplModuleIndex,
    /// Like [`trait_impl_modules`] but restricted to **fully concrete**
    /// impls — `impl <Trait> for <Type> { … }` blocks whose `type_params`
    /// are empty. This mirrors the pre-Step-4 monomorphizer index, which
    /// only catalogued trait methods belonging to non-generic, non-blanket
    /// impl blocks. Mono needs this distinction to decide which
    /// `module_source` a substituted trait-method call should carry: a
    /// generic impl's instantiation lives in the receiver type's module
    /// (preserving the legacy convention), while a concrete impl's
    /// function lives in the impl block's module.
    pub(crate) concrete_trait_impl_modules: TraitImplModuleIndex,
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
    /// `(type_name, trait_name)` → `ModuleSource` for synthesized non-blanket
    /// trait impls (auto-derives plus the impls produced by `from_synth` /
    /// `serde_synth`). Mirrors the AST-level `trait_impl_modules` shape and
    /// participates in the same lookup via [`TraitEnv::impl_module_for`].
    /// Includes both concrete (e.g. auto-derived `Inspect for Wrapper`) and
    /// generic synthesised impls.
    pub trait_impl_modules: TraitImplModuleIndex,
    /// Concrete-only subset (no impl-block type parameters). Mirrors the
    /// AST-level [`TraitEnv::concrete_trait_impl_modules`] field; see its
    /// docs for why mono needs to distinguish concrete impls from generic
    /// ones.
    pub concrete_trait_impl_modules: TraitImplModuleIndex,
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
        type_name: String,
        trait_name: String,
        module: ModuleSource,
        is_concrete: bool,
    ) {
        let key = (type_name, trait_name);
        if is_concrete {
            // Concrete impls populate both views; clone once for the
            // duplicated entry. Generic impls only appear in the all-impls
            // view, so they avoid the clone entirely.
            let modules = self
                .concrete_trait_impl_modules
                .entry(key.clone())
                .or_default();
            if !modules.contains(&module) {
                modules.push(module.clone());
            }
        }
        let modules = self.trait_impl_modules.entry(key).or_default();
        if !modules.contains(&module) {
            modules.push(module);
        }
    }

    /// `true` if `impl <trait_name> for <type_name>` has already been
    /// recorded in this synthesis layer (regardless of concreteness).
    #[must_use]
    pub fn has_impl(&self, type_name: &str, trait_name: &str) -> bool {
        self.trait_impl_modules
            .contains_key(&(type_name.to_string(), trait_name.to_string()))
    }
}

impl TraitEnv {
    /// Build trait indices from all loaded modules.
    ///
    /// Called once in `resolve_all_modules` before per-module resolution begins.
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
    ) -> (Arc<Self>, Vec<TypeError>) {
        let mut impl_index: TraitImplIndex = IndexMap::default();
        let mut decl_index: TraitDeclIndex = IndexMap::default();
        let mut effect_decl_index: EffectDeclIndex = IndexMap::default();
        let mut resource_decl_index: ResourceDeclIndex = IndexMap::default();
        let mut blanket_impl_index: BlanketTraitImplIndex = Vec::new();
        let mut blanket_trait_impl_modules: IndexMap<String, Vec<ModuleSource>> =
            IndexMap::default();
        let mut trait_impl_modules: TraitImplModuleIndex = IndexMap::default();
        let mut concrete_trait_impl_modules: TraitImplModuleIndex = IndexMap::default();
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
            for (item_idx, item) in module.items.iter().enumerate() {
                match item {
                    Item::Trait(trait_decl) => {
                        // `(module_source, name)` key: two modules can declare
                        // a same-named trait without colliding. The previous
                        // bare-name key first-wrote-wins and silently routed
                        // both declarations to the same entry.
                        decl_index.insert(
                            (module_source.clone(), trait_decl.name.clone()),
                            (module_source.clone(), item_idx),
                        );
                    }
                    Item::Interface(effect_decl) => {
                        effect_decl_index.insert(
                            (module_source.clone(), effect_decl.name.clone()),
                            (module_source.clone(), item_idx),
                        );
                    }
                    Item::Resource(resource) => {
                        let resource_key = (module_source.clone(), resource.name.clone());
                        resource_decl_index.insert(
                            resource_key.clone(),
                            (module_source.clone(), item_idx),
                        );
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
                                        item_idx,
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

        // Canonicalise a bare type / trait name referenced in
        // `module_source` (an impl block's home module). Looks first
        // through `symbols` (per-module imports + that module's own
        // declarations), then falls back to scanning the decl indices
        // built above so that prelude-implicit names (`Eq`, `Ord`,
        // `Display`, …) resolve to their declaring module even when
        // the impl's module never `use`'d them. The final fallback —
        // `(module_source, name)` — applies to blanket type parameters
        // and to references whose target wasn't registered as a
        // top-level declaration.
        let canonical_key = |module_source: &ModuleSource, name: &str| -> DeclKey {
            // Built-in primitives (`i32`, `f64`, `char`, …) have no
            // `Symbol` entry and no decl item, but they share a known
            // canonical module: `core:prelude/primitive`. Lookups via
            // `Resolver::canonical_decl_key` use the same shortcut, so
            // every inherent `impl <primitive> { … }` block keys into
            // the same bucket regardless of which file the impl lives in.
            if super::is_primitive_type_name(name) {
                return (ModuleSource::primitive(), name.to_string());
            }
            if let Some(sym) = symbols.lookup_in_module(module_source, name) {
                return (sym.defined_at.module.clone(), sym.name.clone());
            }
            if let Some(key) = decl_index.keys().find(|(_, n)| n == name) {
                return key.clone();
            }
            if let Some(key) = effect_decl_index.keys().find(|(_, n)| n == name) {
                return key.clone();
            }
            if let Some(key) = resource_decl_index.keys().find(|(_, n)| n == name) {
                return key.clone();
            }
            if let Some(key) = type_decl_index.keys().find(|(_, n)| n == name) {
                return key.clone();
            }
            (module_source.clone(), name.to_string())
        };

        // Pass 2: walk impl blocks now that all decl indices are
        // populated, so the per-impl canonicalisation above can resolve
        // every PascalCase reference to its declaring module.
        for (module_source, module) in modules {
            for (item_idx, item) in module.items.iter().enumerate() {
                let Item::Impl(impl_block) = item else { continue };
                let type_name = get_type_name_static(&impl_block.ty);
                if let Some(trait_type) = &impl_block.trait_type {
                    let trait_name = get_type_name_static(trait_type);
                    let is_blanket = impl_block
                        .type_params
                        .iter()
                        .any(|tp| tp.name == type_name && !tp.bounds.is_empty());
                    if is_blanket {
                        blanket_impl_index.push((module_source.clone(), item_idx));
                        let modules =
                            blanket_trait_impl_modules.entry(trait_name).or_default();
                        if !modules.contains(module_source) {
                            modules.push(module_source.clone());
                        }
                    } else {
                        let key = (type_name.clone(), trait_name);
                        let modules = trait_impl_modules.entry(key.clone()).or_default();
                        if !modules.contains(module_source) {
                            modules.push(module_source.clone());
                        }
                        // Track the concrete subset separately: only impl
                        // blocks with no type parameters at all qualify
                        // (e.g. `impl Display for String`, not
                        // `impl<T> Inspect for Array<T>`).
                        if impl_block.type_params.is_empty() {
                            let cmodules =
                                concrete_trait_impl_modules.entry(key).or_default();
                            if !cmodules.contains(module_source) {
                                cmodules.push(module_source.clone());
                            }
                        }
                    }
                    impl_index
                        .entry(type_name.clone())
                        .or_default()
                        .push((module_source.clone(), item_idx));
                    // Static methods on trait impl blocks (no `self`
                    // parameter) join the same canonical bucket as
                    // inherent statics. `f64::from_bits` and friends in
                    // `core:prelude/int128.wado` flow through this path.
                    let recv_key = canonical_key(module_source, &type_name);
                    for (method_idx, method) in impl_block.methods.iter().enumerate() {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if !has_self {
                            static_method_index
                                .entry(recv_key.clone())
                                .or_default()
                                .push((
                                    method.name.clone(),
                                    module_source.clone(),
                                    item_idx,
                                    method_idx,
                                ));
                        }
                    }
                } else {
                    // Non-trait (inherent) impl block: index static methods.
                    let recv_key = canonical_key(module_source, &type_name);
                    for (method_idx, method) in impl_block.methods.iter().enumerate() {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if !has_self {
                            static_method_index
                                .entry(recv_key.clone())
                                .or_default()
                                .push((
                                    method.name.clone(),
                                    module_source.clone(),
                                    item_idx,
                                    method_idx,
                                ));
                        }
                    }
                }
            }
        }

        let violations = check_all_orphan_rules(modules, &decl_index, &type_decl_index);

        (
            Arc::new(Self {
                impl_index,
                decl_index,
                effect_decl_index,
                resource_decl_index,
                blanket_impl_index,
                blanket_trait_impl_modules,
                static_method_index,
                resource_static_method_index,
                trait_impl_modules,
                concrete_trait_impl_modules,
                synthesised: None,
            }),
            violations,
        )
    }

    /// Look up the module that defines `impl <trait_name> for <type_name>`.
    /// Consults the AST layer first, then the synthesis layer (if populated).
    /// Returns `None` for blanket impls and types not represented as a
    /// concrete impl (e.g. anonymous function types whose `Inspect` is
    /// auto-derived per-module by the synthesis phase).
    ///
    /// When the same simple type name is implemented in multiple modules
    /// (e.g. two `struct Widget` blocks, each auto-derived to `Widget^Inspect`
    /// in its own module), `type_module` disambiguates by preferring an
    /// entry whose module matches the hint. This mirrors how the resolver's
    /// `find_trait_impl_for_type_with_args` iterates [`TraitImplIndex`] and
    /// checks each candidate impl individually instead of collapsing on the
    /// `(name, trait)` key.
    pub(crate) fn impl_module_for(
        &self,
        type_name: &str,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        let key = (type_name.to_string(), trait_name.to_string());
        let ast = self.trait_impl_modules.get(&key);
        let syn = self
            .synthesised
            .as_ref()
            .and_then(|s| s.trait_impl_modules.get(&key));
        pick_module_union(ast, syn, type_module)
    }

    /// Return the home module of a blanket impl (`impl<T: Bound> Trait for T`)
    /// for `trait_name`, if one exists. When multiple blanket impls
    /// implement the same trait, the first registered module is returned,
    /// with `type_module` preferred when present (used as a stable
    /// tie-breaker).
    pub(crate) fn blanket_impl_module_for_trait(
        &self,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        let modules = self.blanket_trait_impl_modules.get(trait_name)?;
        if let Some(hint) = type_module
            && let Some(m) = modules.iter().find(|m| *m == hint)
        {
            return Some(m);
        }
        modules.first()
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
        type_name: &str,
        trait_name: &str,
        type_module: Option<&ModuleSource>,
    ) -> Option<&ModuleSource> {
        let key = (type_name.to_string(), trait_name.to_string());
        let ast = self.concrete_trait_impl_modules.get(&key);
        let syn = self
            .synthesised
            .as_ref()
            .and_then(|s| s.concrete_trait_impl_modules.get(&key));
        pick_module_union(ast, syn, type_module)
    }

    /// Find the canonical `(declaring module, name)` key for any trait
    /// declaration with the given bare name. Returns `None` if no module
    /// declares a trait by this name.
    ///
    /// Linear scan over `decl_index`. Intended for synthesis sites
    /// (auto-derived `Inspect` / `Display` / `Eq`, `serde` adapters, …)
    /// that have only a bare trait name (e.g. via `compiler_items()`) and
    /// no per-module import context to canonicalise it through
    /// `Resolver::canonical_decl_key`. When two modules declare same-
    /// named traits the first match is returned; the synthesis path that
    /// uses this is well-defined only for core traits whose name is
    /// project-globally unique, so the ambiguity does not matter in
    /// practice.
    pub fn find_trait_decl_key(&self, name: &str) -> Option<DeclKey> {
        self.decl_index.keys().find(|(_, n)| n == name).cloned()
    }

    /// Find the canonical `(declaring module, name)` key for any effect
    /// or resource declaration with the given bare name. See
    /// [`Self::find_trait_decl_key`] for the lookup contract.
    pub fn find_effect_or_resource_decl_key(&self, name: &str) -> Option<DeclKey> {
        self.effect_decl_index
            .keys()
            .chain(self.resource_decl_index.keys())
            .find(|(_, n)| n == name)
            .cloned()
    }

    /// Convenience for synthesis sites: build a `(declaring module, name)`
    /// pair for use as [`crate::name::LocalMethodName::base_trait`]. Falls
    /// back to [`ModuleSource::prelude`] when no module declares a trait
    /// by this name — that covers the compiler-internal `Fn` family and
    /// any future trait that synthesis references before its prelude
    /// declaration is registered. The fallback's module is acceptable
    /// because the only code that consumes `base_trait_module` to
    /// disambiguate is dispatch synthesis, which keys the effect /
    /// resource indices by `(module, name)` and treats a non-match as
    /// "not an effect / resource".
    pub fn trait_ref_for(&self, trait_name: &str) -> DeclKey {
        self.find_trait_decl_key(trait_name)
            .unwrap_or_else(|| (ModuleSource::prelude(), trait_name.to_string()))
    }

    /// Find any canonical receiver [`DeclKey`] currently registered in
    /// the static-method index whose bare name matches `name`. Used as a
    /// final fallback in [`crate::resolver::Resolver::canonical_decl_key`]
    /// for receiver names the per-module import context and symbol table
    /// can't canonicalise — most commonly the built-in primitives
    /// (`char`, `i32`, `f64`, …) which have `impl <prim> { … }` blocks in
    /// `core:prelude/primitive` but no `Symbol` entry to consult through
    /// `lookup_in_module`.
    pub fn find_static_method_decl_key(&self, name: &str) -> Option<DeclKey> {
        self.static_method_index
            .keys()
            .chain(self.resource_static_method_index.keys())
            .find(|(_, n)| n == name)
            .cloned()
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

/// Returns `true` if the module source is a user-local module (part of the current package).
fn is_user_local(ms: &ModuleSource) -> bool {
    matches!(
        ms,
        ModuleSource::Local { .. }
            | ModuleSource::EntryPoint { .. }
            | ModuleSource::Redirected { .. }
    )
}

/// Returns `true` if the named type is a local (user-defined) type.
/// Primitive types (i32, bool, char, etc.) are not in the set and return `false`.
fn is_local_type_name(name: &str, local_type_names: &IndexSet<String>) -> bool {
    local_type_names.contains(name)
}

/// Returns `true` if the named trait is a local (user-defined) trait.
/// Any module that declares a trait by this name suffices; per-module
/// disambiguation is not required because the orphan rule operates at the
/// project (user-local) granularity.
fn is_local_trait_name(name: &str, local_trait_names: &IndexSet<String>) -> bool {
    local_trait_names.contains(name)
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
    type_params: &[String],
    local_type_names: &IndexSet<String>,
) -> PositionKind {
    match ty {
        // Fundamental: look through references
        Type::Reference(inner) | Type::MutReference(inner) => {
            classify_position(inner, type_params, local_type_names)
        }
        Type::Named(named) => {
            if type_params.contains(&named.name) {
                PositionKind::UncoveredTypeParam
            } else if is_local_type_name(&named.name, local_type_names) {
                PositionKind::LocalType
            } else {
                PositionKind::ForeignType
            }
        }
        Type::Generic(generic) => {
            if type_params.contains(&generic.name) {
                // Generic<T> where generic itself is a type param: uncovered
                PositionKind::UncoveredTypeParam
            } else if is_local_type_name(&generic.name, local_type_names) {
                // LocalType<...>: the head is local → this position is local
                PositionKind::LocalType
            } else {
                PositionKind::ForeignType
            }
        }
        // Tuples are local if the current crate owns them (via `pub type [..T];`)
        Type::Tuple(_) => {
            if local_type_names.contains(TypeTable::TUPLE_TYPE_NAME) {
                PositionKind::LocalType
            } else {
                PositionKind::ForeignType
            }
        }
        Type::Function(_) | Type::NamespacedGeneric(_) | Type::TypePackSpread(..) => {
            PositionKind::ForeignType
        }
    }
}

/// Check the RFC 2451 orphan rule for a single impl block that has a foreign trait.
///
/// Sequence: `[self_type, trait_arg1, trait_arg2, ...]`.
/// Valid if there exists a position with `LocalType` and no `UncoveredTypeParam` before it.
fn check_orphan_rfc2451(impl_block: &ast::ImplBlock, local_type_names: &IndexSet<String>) -> bool {
    let type_params: Vec<String> = impl_block
        .type_params
        .iter()
        .map(|p| p.name.clone())
        .collect();

    // Build the sequence: self type first, then trait type arguments
    let trait_args: &[Type] = match impl_block.trait_type.as_ref() {
        Some(Type::Generic(g)) => &g.args,
        _ => &[],
    };

    let mut seen_uncovered_before_local = false;

    // Position 0: self type
    match classify_position(&impl_block.ty, &type_params, local_type_names) {
        PositionKind::LocalType => return true,
        PositionKind::UncoveredTypeParam => seen_uncovered_before_local = true,
        PositionKind::ForeignType => {}
    }

    // Positions 1+: trait type arguments
    for trait_arg in trait_args {
        match classify_position(trait_arg, &type_params, local_type_names) {
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

/// Check orphan rules for all trait impl blocks across all modules.
/// Only impl blocks in local (user) modules are checked.
fn check_all_orphan_rules(
    modules: &IndexMap<ModuleSource, Module>,
    decl_index: &TraitDeclIndex,
    type_decl_index: &IndexMap<DeclKey, ModuleSource>,
) -> Vec<TypeError> {
    let mut violations = Vec::new();

    // Project all (`DeclKey`, `ModuleSource`) entries down to bare names of
    // user-local declarations. The orphan rule applies at the project /
    // user-local granularity, so the sets need only track *whether* a name
    // is defined by some user module — per-module disambiguation isn't
    // needed for orphan classification (and would require canonicalising
    // an impl block's referenced type, which lacks the module context the
    // build phase has access to).
    let local_type_names: IndexSet<String> = type_decl_index
        .iter()
        .filter(|(_, ms)| is_user_local(ms))
        .map(|((_, name), _)| name.clone())
        .collect();
    let local_trait_names: IndexSet<String> = decl_index
        .iter()
        .filter(|(_, (ms, _))| is_user_local(ms))
        .map(|((_, name), _)| name.clone())
        .collect();

    for (module_source, module) in modules {
        if !is_user_local(module_source) {
            continue;
        }

        for item in &module.items {
            let Item::Impl(impl_block) = item else {
                continue;
            };
            let Some(trait_type) = &impl_block.trait_type else {
                continue; // inherent impl, no orphan rule
            };

            let trait_name = get_type_name_static(trait_type);

            // If the trait is local, always allowed
            if is_local_trait_name(&trait_name, &local_trait_names) {
                continue;
            }

            // Foreign trait: apply RFC 2451 sequence check
            if !check_orphan_rfc2451(impl_block, &local_type_names) {
                let self_type_name = get_type_name_static(&impl_block.ty);
                violations.push(TypeError::OrphanViolation {
                    trait_name,
                    self_type_name,
                    span: impl_block.span,
                });
            }
        }
    }

    violations
}

/// Mutable trait resolution context scoped to the current resolution site.
///
/// Groups all state that changes when entering/leaving generic scopes
/// (impl blocks, trait method lookups, etc). Use [`Resolver::enter_fresh_type_param_scope`]
/// or [`Resolver::enter_inherited_type_param_scope`] to mutate this safely with RAII
/// restore on drop.
#[derive(Clone, Default)]
pub(super) struct TraitContext {
    /// Type parameters currently in scope (name → (index, `TypeId`)).
    /// Set when resolving generic structs, functions, or impl blocks.
    pub(super) type_params: IndexMap<String, (u32, TypeId)>,
    /// `AstId` of each type param's declaration site (for LSP jump-to-def on
    /// type-parameter uses). Parallel to `type_params`, keyed by name.
    pub(super) type_param_decls: IndexMap<String, ast::AstId>,
    /// Trait bounds on type parameters in scope (name → full bounds with assoc types).
    /// Used for resolving trait methods on type params (e.g., `T.cmp()` when T: Ord).
    pub(super) type_param_bounds: IndexMap<String, Vec<ast::TraitBound>>,
    /// Associated type bindings in scope (`Self::Name` → resolved type).
    /// Set when resolving trait implementations.
    pub(super) assoc_type_bindings: IndexMap<String, TypeId>,
    /// Current `Self` type in scope (the type being implemented in an impl block).
    pub(super) self_type: Option<TypeId>,
}

/// RAII guard that restores `Resolver::trait_ctx` to its saved value on drop.
///
/// Implements `Deref<Target = Resolver>` so it can be used as a transparent
/// resolver handle inside the scope. Restoration is panic-safe: even if the
/// scope body panics, drop still runs and the parent context is reinstated.
///
/// Use [`Resolver::enter_inherited_type_param_scope`] to enter a new scope.
/// It preserves the current `trait_ctx` so the child scope can register new
/// entries on top of the parent's. Callers that want a clean slate for a
/// specific field (matching the legacy `mem::take` pattern) should clear that
/// field on `scope.trait_ctx` after entering.
pub(super) struct TypeParamScope<'r, 'a, H: CompilerHost> {
    resolver: &'r mut Resolver<'a, H>,
    saved: TraitContext,
}

impl<'a, H: CompilerHost> Deref for TypeParamScope<'_, 'a, H> {
    type Target = Resolver<'a, H>;
    fn deref(&self) -> &Resolver<'a, H> {
        self.resolver
    }
}

impl<'a, H: CompilerHost> DerefMut for TypeParamScope<'_, 'a, H> {
    fn deref_mut(&mut self) -> &mut Resolver<'a, H> {
        self.resolver
    }
}

impl<H: CompilerHost> TypeParamScope<'_, '_, H> {
    /// Access the saved (parent) `TraitContext`. Useful when setting up an
    /// inner scope for an impl block whose impl type refers to one of the
    /// parent's type params (blanket impl / `&T` impl / variadic impl).
    pub(super) fn saved(&self) -> &TraitContext {
        &self.saved
    }
}

impl<H: CompilerHost> Drop for TypeParamScope<'_, '_, H> {
    fn drop(&mut self) {
        self.resolver.trait_ctx = std::mem::take(&mut self.saved);
    }
}

impl<'a, H: CompilerHost> Resolver<'a, H> {
    /// Enter an inherited type-param scope. The current `trait_ctx` is cloned
    /// into the saved slot, but left in place so the inner work can register
    /// additional type params on top of what the parent already had. The
    /// original context is restored when the returned guard is dropped.
    ///
    /// Callers that want a clean slate (matching the legacy
    /// `mem::take(&mut self.trait_ctx.type_params)` pattern) should clear the
    /// specific fields they want to reset on `scope.trait_ctx` after entering
    /// the scope — only the fields they touch need to be cleared, all others
    /// are inherited from the parent scope.
    pub(super) fn enter_inherited_type_param_scope(&mut self) -> TypeParamScope<'_, 'a, H> {
        let saved = self.trait_ctx.clone();
        TypeParamScope {
            resolver: self,
            saved,
        }
    }

    /// Register a list of generic parameters as `TypeParam` / `TypePack` ids
    /// in the current `trait_ctx`, starting from `offset`. Skips effect params.
    /// Returns the next free index (i.e. `offset + non_effect_count`).
    ///
    /// Trait bounds attached to each parameter are also recorded in
    /// `type_param_bounds` so trait-method lookups on the parameter work.
    pub(super) fn register_generic_params(
        &mut self,
        params: &[ast::GenericParam],
        offset: u32,
    ) -> u32 {
        let mut idx = offset;
        for tp in params.iter().filter(|p| !p.is_effect) {
            // `<F: fn(...)>` / `<F: fn mut(...)>` binds the parameter directly
            // to the bound's function type. The closure-type bound is just
            // surface syntax for "F is exactly this signature" — eager
            // substitution lets `f: F` be callable inside the body and folds
            // every callsite onto the same shared canonical closure shape.
            //
            // Fn-bound params do NOT consume a `TypeParam` index slot. This
            // keeps the index space dense for real type params so the
            // substitution map in `substitute_type_params` (which is keyed by
            // `TypeParam.index`) lines up with the positional order used by
            // the inference cache. Without this, mixed declarations like
            // `<F: fn(...), T>` would leave `T` at `TypeParam(_, 1)` while
            // the cache placed it at position 0, breaking substitution.
            let fn_bound_sig = if tp.is_pack {
                None
            } else {
                tp.bounds.iter().find_map(|b| b.fn_signature.as_ref())
            };
            let (type_id, consumed_index) = if tp.is_pack {
                (
                    self.type_table
                        .borrow_mut()
                        .make_type_pack(tp.name.clone(), idx),
                    true,
                )
            } else if let Some(sig) = fn_bound_sig {
                (self.resolve_type(&ast::Type::Function(sig.clone())), false)
            } else {
                (
                    self.type_table
                        .borrow_mut()
                        .make_type_param(tp.name.clone(), idx),
                    true,
                )
            };
            self.trait_ctx
                .type_params
                .insert(tp.name.clone(), (idx, type_id));
            self.trait_ctx
                .type_param_decls
                .insert(tp.name.clone(), tp.id);
            // Filter out `fn`/`fn mut` bounds before recording (they're already
            // realised in the bound type itself); only "real" trait bounds need
            // remembering for method lookup.
            let real_bounds: Vec<ast::TraitBound> = tp
                .bounds
                .iter()
                .filter(|b| b.fn_signature.is_none())
                .cloned()
                .collect();
            if !real_bounds.is_empty() {
                self.trait_ctx
                    .type_param_bounds
                    .insert(tp.name.clone(), real_bounds);
            }
            if consumed_index {
                idx += 1;
            }
        }
        idx
    }

    /// Bind a trait's declared type parameters to the impl's concrete trait
    /// arguments in the current scope. Given `trait Foo<T, U>` and an impl
    /// instance `Foo<i32, String>`, this registers `T → i32` and `U → String`
    /// in `trait_ctx.type_params` together with their declared bounds.
    ///
    /// Callers must have any impl-level type params already registered because
    /// the trait args may reference them (e.g., `impl<X> Foo<Container<X>>`).
    /// Trait args are resolved in the current scope before being inserted.
    ///
    /// Entries already present in `type_params` (e.g., re-used impl-level
    /// names) are left untouched.
    pub(super) fn bind_trait_type_params_from_impl(&mut self, trait_type: &ast::Type) {
        let trait_name = self.get_type_name(trait_type);
        let Some(trait_decl_type_params) = self.find_trait_decl_type_params(&trait_name) else {
            return;
        };
        let trait_args: Vec<&ast::Type> = match trait_type {
            ast::Type::Generic(g) => g.args.iter().collect(),
            _ => Vec::new(),
        };
        for (i, tp) in trait_decl_type_params
            .iter()
            .filter(|p| !p.is_effect)
            .enumerate()
        {
            if self.trait_ctx.type_params.contains_key(&tp.name) {
                continue;
            }
            let Some(arg_ast) = trait_args.get(i) else {
                continue;
            };
            let resolved_arg = self.resolve_type(arg_ast);
            let idx = self.trait_ctx.type_params.len() as u32;
            self.trait_ctx
                .type_params
                .insert(tp.name.clone(), (idx, resolved_arg));
            if !tp.bounds.is_empty() {
                self.trait_ctx
                    .type_param_bounds
                    .entry(tp.name.clone())
                    .or_default()
                    .extend(tp.bounds.clone());
            }
        }
    }
}

/// Extract a type name from an AST type without needing a Resolver instance.
fn get_type_name_static(ty: &ast::Type) -> String {
    match ty {
        ast::Type::Named(named) if named.name == "()" => TypeTable::UNIT_TYPE_NAME.to_string(),
        ast::Type::Named(named) => named.name.clone(),
        ast::Type::Generic(generic) => generic.name.clone(),
        ast::Type::Reference(_) => "&".to_string(),
        ast::Type::MutReference(_) => "&mut".to_string(),
        ast::Type::Tuple(elems) => {
            if elems.is_empty() {
                TypeTable::UNIT_TYPE_NAME.to_string()
            } else {
                TypeTable::TUPLE_TYPE_NAME.to_string()
            }
        }
        _ => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{GenericParam, GenericType, ImplBlock, NamedType};
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
            source_interface: None,
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

    fn make_type_decl_index(local_names: &[&str]) -> IndexSet<String> {
        let mut s = IndexSet::default();
        for &name in local_names {
            s.insert(name.to_string());
        }
        s
    }

    fn impl_block(type_params: Vec<GenericParam>, trait_type: Type, self_type: Type) -> ImplBlock {
        ImplBlock {
            id: crate::ast::AstId::fresh(),
            type_params,
            trait_type: Some(trait_type),
            ty: self_type,
            associated_types: vec![],
            constants: vec![],
            methods: vec![],
            is_synthesize_request: false,
            has_rest: false,
            span: dummy_span(),
        }
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
        let tdx = make_type_decl_index(&["MyError"]);
        assert!(matches!(
            classify_position(&named("MyError"), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_foreign_named_type() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&named("String"), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_primitive_is_foreign() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&named("i32"), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_uncovered_type_param() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&named("T"), &["T".to_string()], &tdx),
            PositionKind::UncoveredTypeParam
        ));
    }

    #[test]
    fn test_classify_local_generic_head_is_local() {
        // LocalType<T> — head is local regardless of args
        let tdx = make_type_decl_index(&["LocalType"]);
        let ty = generic("LocalType", vec![named("T")]);
        assert!(matches!(
            classify_position(&ty, &["T".to_string()], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_foreign_generic_is_foreign() {
        // Array<T> — head Array is foreign
        let tdx = make_type_decl_index(&[]);
        let ty = generic("Array", vec![named("T")]);
        assert!(matches!(
            classify_position(&ty, &["T".to_string()], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_reference_to_local_is_local() {
        // &LocalType — fundamental: look through &
        let tdx = make_type_decl_index(&["MyStruct"]);
        assert!(matches!(
            classify_position(&ref_type(named("MyStruct")), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_mut_reference_to_local_is_local() {
        // &mut LocalType — fundamental: look through &mut
        let tdx = make_type_decl_index(&["MyStruct"]);
        assert!(matches!(
            classify_position(&mut_ref_type(named("MyStruct")), &[], &tdx),
            PositionKind::LocalType
        ));
    }

    #[test]
    fn test_classify_reference_to_foreign_is_foreign() {
        let tdx = make_type_decl_index(&[]);
        assert!(matches!(
            classify_position(&ref_type(named("String")), &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    #[test]
    fn test_classify_tuple_is_foreign() {
        // Tuple types have no single named head → foreign
        let tdx = make_type_decl_index(&["MyStruct"]);
        let ty = Type::Tuple(vec![named("MyStruct"), named("i32")]);
        assert!(matches!(
            classify_position(&ty, &[], &tdx),
            PositionKind::ForeignType
        ));
    }

    // --- check_orphan_rfc2451 ---

    #[test]
    fn test_rfc2451_local_self_type_allowed() {
        // impl ForeignTrait for LocalType → T0 is local → allowed
        let tdx = make_type_decl_index(&["MyStruct"]);
        let ib = impl_block(vec![], named("ForeignTrait"), named("MyStruct"));
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_both_foreign_forbidden() {
        // impl Eq for String → both foreign
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(vec![], named("Eq"), named("String"));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_local_in_trait_arg_allowed() {
        // impl From<MyError> for String → T0=String(foreign), T1=MyError(local) → allowed
        let tdx = make_type_decl_index(&["MyError"]);
        let ib = impl_block(
            vec![],
            generic("From", vec![named("MyError")]),
            named("String"),
        );
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_uncovered_type_param_forbidden() {
        // impl<T> Eq for T → T0=T(uncovered) → forbidden
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(vec![type_param("T")], named("Eq"), named("T"));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_uncovered_param_before_local_in_trait_arg_forbidden() {
        // impl<T> From<T> for String → T0=String(foreign), T1=T(uncovered) → forbidden
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(
            vec![type_param("T")],
            generic("From", vec![named("T")]),
            named("String"),
        );
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_local_type_as_generic_head_in_trait_arg() {
        // impl<T> From<LocalType<T>> for ForeignType → T0=ForeignType, T1=LocalType<T>(local head) → allowed
        let tdx = make_type_decl_index(&["LocalType"]);
        let trait_ty = generic("From", vec![generic("LocalType", vec![named("T")])]);
        let ib = impl_block(vec![type_param("T")], trait_ty, named("ForeignType"));
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_foreign_generic_head_in_trait_arg_forbidden() {
        // impl<T> From<Array<T>> for ForeignType → T0=ForeignType, T1=Array<T>(foreign head) → forbidden
        let tdx = make_type_decl_index(&[]);
        let trait_ty = generic("From", vec![generic("Array", vec![named("T")])]);
        let ib = impl_block(vec![type_param("T")], trait_ty, named("ForeignType"));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_ref_to_local_as_self_type() {
        // impl ForeignTrait for &LocalType → fundamental, look through & → allowed
        let tdx = make_type_decl_index(&["MyStruct"]);
        let ib = impl_block(vec![], named("ForeignTrait"), ref_type(named("MyStruct")));
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_ref_to_foreign_as_self_type_forbidden() {
        // impl ForeignTrait for &String → &String is foreign → forbidden
        let tdx = make_type_decl_index(&[]);
        let ib = impl_block(vec![], named("ForeignTrait"), ref_type(named("String")));
        assert!(!check_orphan_rfc2451(&ib, &tdx));
    }

    #[test]
    fn test_rfc2451_local_self_before_uncovered_param_in_trait_arg() {
        // impl<T> From<T> for LocalType → T0=LocalType(local!) → allowed before reaching T1=T
        let tdx = make_type_decl_index(&["LocalType"]);
        let ib = impl_block(
            vec![type_param("T")],
            generic("From", vec![named("T")]),
            named("LocalType"),
        );
        assert!(check_orphan_rfc2451(&ib, &tdx));
    }
}
