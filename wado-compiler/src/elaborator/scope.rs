//! Transient annotate-walk scope and its RAII guards.
//!
//! Rule: scope state is mutated only through the guards and `with_*`
//! helpers in this file — every entry has exactly one panic-safe restore
//! path (WEP 2026-05-26).

use std::borrow::Borrow;
use std::cell::{Cell, RefCell};
use std::hash::Hash;
use std::ops::{Deref, DerefMut};

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::TypeId;

use super::Elaborator;
use super::trait_env::{InheritedBound, ViaClause};
use super::trait_query::SelfBinding;
use super::types::TypeError;
use super::util;
use crate::defs::DefId;
use crate::name::FqTypeName;
use crate::token::Span;

/// A name bound in a type-parameter scope: its slot, the type it stands for,
/// and the node that declares it.
#[derive(Clone, Copy, Debug)]
pub(super) struct BinderInScope {
    /// Slot in the enclosing item's parameter list.
    pub(super) index: u32,
    pub(super) type_id: TypeId,
    /// Where the parameter is written, for jump-to-def on a use. `None` for a
    /// name no parameter declares: `Self`, or one already bound to a concrete
    /// type. Not what names a binder in a mangle — see
    /// [`TraitContext::impl_owner`].
    pub(super) decl: Option<ast::AstId>,
}

impl BinderInScope {
    /// A binder the source declares, named by its own node.
    pub(super) fn declared(index: u32, type_id: TypeId, decl: ast::AstId) -> Self {
        Self {
            index,
            type_id,
            decl: Some(decl),
        }
    }

    /// A name in scope that no parameter declares — see [`Self::decl`].
    pub(super) fn undeclared(index: u32, type_id: TypeId) -> Self {
        Self {
            index,
            type_id,
            decl: None,
        }
    }
}

/// A bound a frame carries, with the chain it came through when it was reached
/// through a supertrait clause rather than written here.
///
/// `inherited` names the trait the written bound resolved to and the clauses
/// from there down to the one that declared `bound`. `bound`'s own types are
/// written in that last trait's parameter space, so a reader resolves them
/// through the chain instead of reading them here (WEP-2026-08-12).
#[derive(Clone)]
pub(super) struct ElaboratedBound {
    pub(super) bound: ast::TraitBound,
    pub(super) inherited: Option<(DefId, Vec<ViaClause>)>,
    /// Whose `Self` this bound's written types mean.
    pub(super) self_type: BoundSelf,
}

/// Whose `Self` a bound's written types mean. The two cases are not the same
/// type, so a reader that supplies one for the other resolves a projection off
/// the wrong receiver (#2112).
#[derive(Clone, Copy, Debug)]
pub(super) enum BoundSelf {
    /// The frame that wrote the bound; `None` where that frame binds no `Self`.
    Frame(Option<SelfBinding>),
    /// The type being bounded. A supertrait clause and a declared parameter
    /// default are both written in the trait's own space, where `Self` is
    /// whichever type the bound is standing on.
    Bounded,
}

impl BoundSelf {
    /// The `Self` to resolve under, for a bound standing on `bounded` that
    /// `declaring` wrote.
    pub(super) fn at(self, bounded: TypeId, declaring: DefId) -> Option<SelfBinding> {
        match self {
            Self::Frame(binding) => binding,
            Self::Bounded => Some(SelfBinding {
                type_id: bounded,
                declaring_trait: Some(declaring),
            }),
        }
    }
}

/// A bound together with what `Self` means where it was written, which is not
/// the frame of whatever later reads it (#2112).
#[derive(Clone, Debug)]
pub(super) struct ScopedBound {
    pub(super) bound: ast::TraitBound,
    /// What `Self` meant where the bound was written. `None` in a frame that
    /// binds none, where a `Self`-rooted spelling is rejected at the declaration.
    pub(super) self_binding: Option<SelfBinding>,
}

impl ScopedBound {
    pub(super) fn new(bound: ast::TraitBound, self_binding: Option<SelfBinding>) -> Self {
        Self {
            bound,
            self_binding,
        }
    }

    /// What `param` declares, pinned to one frame's `Self`. An `fn` bound is left
    /// out: it is realised in the parameter's own type, so nothing reaches it
    /// through a trait name, which is the only way `type_param_bounds` is read.
    pub(super) fn pin_declared(
        param: &ast::GenericParam,
        self_binding: Option<SelfBinding>,
    ) -> Vec<Self> {
        Self::pin_all(&param.real_bounds(), self_binding)
    }

    /// Pin every bound in `bounds` to one frame's `Self`.
    pub(super) fn pin_all(
        bounds: &[ast::TraitBound],
        self_binding: Option<SelfBinding>,
    ) -> Vec<Self> {
        bounds
            .iter()
            .cloned()
            .map(|bound| Self::new(bound, self_binding))
            .collect()
    }

    /// Whose `Self` this bound's written types mean. A bound written at a frame
    /// means that frame, never the type it is standing on.
    pub(super) fn scope(&self) -> BoundSelf {
        BoundSelf::Frame(self.self_binding)
    }
}

impl Borrow<ast::TraitBound> for ScopedBound {
    fn borrow(&self) -> &ast::TraitBound {
        &self.bound
    }
}

impl Deref for ScopedBound {
    type Target = ast::TraitBound;

    fn deref(&self) -> &ast::TraitBound {
        &self.bound
    }
}

/// A trait declaration's own parameter, as the site supplying its argument
/// sees it.
pub(super) struct TraitParamFromImpl<'p, 'a, A> {
    pub(super) param: &'p ast::GenericParam,
    /// What the site wrote at this parameter's argument position, `None` where
    /// it wrote none. What a missing argument means is the caller's: a default
    /// to expand, or a parameter to leave alone.
    pub(super) arg: Option<&'a A>,
    /// The slot the parameter occupies in the trait's own numbering, counted
    /// from 1 since slot 0 is the trait's `Self`.
    pub(super) slot: u32,
    /// Whether the parameter occupies `slot` at all. An `fn`-bound one is
    /// realised in its own type, so it takes an argument position and no slot.
    pub(super) takes_a_slot: bool,
    pub(super) bounds: Vec<ScopedBound>,
}

/// Each parameter of a trait declaration with the argument a site wrote for
/// it, both its numberings, and its bounds pinned to `implementing`.
pub(super) fn trait_params_from_impl<'p, 'a, A>(
    params: &'p [ast::GenericParam],
    args: &'a [A],
    implementing: Option<SelfBinding>,
) -> Vec<TraitParamFromImpl<'p, 'a, A>> {
    let mut slot = 1;
    params
        .iter()
        .filter(|param| param.fills_impl_slot())
        .enumerate()
        .map(|(at, param)| {
            let takes_a_slot = param.is_real_type_param();
            let this = TraitParamFromImpl {
                param,
                arg: args.get(at),
                slot,
                takes_a_slot,
                bounds: ScopedBound::pin_declared(param, implementing),
            };
            slot += u32::from(takes_a_slot);
            this
        })
        .collect()
}

/// The node in `params` that declares `name`, when one does. The caller picks
/// the list, since only it knows which item bound the name (#1932).
pub(super) fn param_decl(params: &[ast::GenericParam], name: &str) -> Option<ast::AstId> {
    params.iter().find(|p| p.name == name).map(|p| p.id)
}

/// Mutable trait resolution context scoped to the current resolution site.
///
/// Groups all state that changes when entering/leaving generic scopes
/// (impl blocks, trait method lookups, etc). Use
/// [`Elaborator::enter_inherited_type_param_scope`] to mutate this safely
/// with RAII restore on drop.
#[derive(Clone, Default)]
pub(super) struct TraitContext {
    /// Type parameters currently in scope. Set when resolving generic structs,
    /// functions, or impl blocks.
    pub(super) type_params: IndexMap<String, BinderInScope>,
    /// Trait bounds on type parameters in scope (name → full bounds with assoc
    /// types), each paired with the `Self` its written types mean. Used for
    /// resolving trait methods on type params (e.g., `T.cmp()` when T: Ord).
    pub(super) type_param_bounds: IndexMap<String, Vec<ScopedBound>>,
    /// Associated type bindings in scope (`Self::Name` → resolved type).
    /// Set when resolving trait implementations.
    pub(super) assoc_type_bindings: IndexMap<String, TypeId>,
    /// Current `Self` type in scope (the type being implemented in an impl block).
    pub(super) self_type: Option<TypeId>,
    /// The trait `Self` is being elaborated against — the trait an `impl` block
    /// names. Qualifies `Self::Assoc` when `Self` is a concrete type, where
    /// there is no `Self` bound to read the declaring trait off.
    pub(super) self_trait: Option<DefId>,
    /// The `impl` block whose type parameters are in scope, paired with the
    /// node declaring its receiver binder — what names that binder in a mangle.
    /// The node, not the spelling: a method parameter may shadow the letter.
    pub(super) impl_owner: Option<(DefId, Option<ast::AstId>)>,
}

/// Everything [`Elaborator::set_self_binding`] installs, so a scoped install
/// takes and restores what `Self` means as one.
pub(super) struct SelfFrame {
    assoc_type_bindings: IndexMap<String, TypeId>,
    self_type: Option<TypeId>,
    self_trait: Option<DefId>,
}

/// One open `type_implements_trait` question.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct TraitCheckFrame {
    pub(super) type_id: TypeId,
    pub(super) trait_: DefId,
    /// The arguments the asking bound wrote. Part of the key: the same trait
    /// at two instantiations is two questions.
    pub(super) wanted: Vec<FqTypeName>,
    /// `Scope::member_edges` when the question was asked.
    pub(super) member_edges: u32,
}

/// Per-function annotate-time scope, bundled so queries take one `&Scope`.
/// Every field is walk-local, so none of it belongs on the shared `TypeSystem`.
#[derive(Default)]
pub(super) struct Scope {
    pub(super) trait_ctx: TraitContext,
    /// The `(type, trait)` questions open above the current one, each with
    /// the member edges taken up to it — see `TraitCheckFrame`.
    pub(super) trait_check_stack: RefCell<Vec<TraitCheckFrame>>,
    /// How many structural-member descents the open questions have taken. A
    /// repeated question is grounded only when one of them lies between the
    /// two askings.
    pub(super) member_edges: Cell<u32>,
    /// The module that wrote the AST being resolved, while that is not this
    /// one — a default expression taken at a site in another module. Names in
    /// it resolve in their author's module, so this replaces the walk's own
    /// frame rather than being tried alongside it (WEP 2026-04-11).
    pub(super) resolving_home: Option<ModuleSource>,
    /// While set, use→def edges are dropped: a speculative walk choosing
    /// among overloads leaves no trace, and the real walk records them.
    pub(super) suppress_reference_recording: bool,
    /// The `(base, assoc)` pairs whose binding is being resolved right now.
    /// Two assoc types bounded through each other have no fixpoint.
    pub(super) assoc_binding_stack: IndexSet<(TypeId, String)>,
    /// The binders whose bound closure is being built right now, since
    /// `T: Uses<T::Item>` asks for it again while it is built.
    pub(super) bound_closure_stack: IndexSet<TypeId>,
}

impl Scope {
    /// Take what `Self` means here, leaving the frame bound to nothing.
    fn take_self_frame(&mut self) -> SelfFrame {
        SelfFrame {
            assoc_type_bindings: std::mem::take(&mut self.trait_ctx.assoc_type_bindings),
            self_type: self.trait_ctx.self_type.take(),
            self_trait: self.trait_ctx.self_trait.take(),
        }
    }

    fn restore_self_frame(&mut self, frame: SelfFrame) {
        self.trait_ctx.assoc_type_bindings = frame.assoc_type_bindings;
        self.trait_ctx.self_type = frame.self_type;
        self.trait_ctx.self_trait = frame.self_trait;
    }
}

/// RAII guard restoring `Elaborator::trait_ctx` on drop, panic-safe. Derefs to
/// the `Elaborator`, so it reads as a transparent handle inside the scope.
/// Entered through [`Elaborator::enter_inherited_type_param_scope`], which keeps
/// the parent's `trait_ctx` in place; a caller wanting a clean slate for one
/// field clears it on `scope.annotate_ctx.trait_ctx` after entering.
pub(super) struct TypeParamScope<'r, 'a, H: CompilerHost> {
    elaborator: &'r mut Elaborator<'a, H>,
    saved: TraitContext,
}

impl<'a, H: CompilerHost> Deref for TypeParamScope<'_, 'a, H> {
    type Target = Elaborator<'a, H>;
    fn deref(&self) -> &Elaborator<'a, H> {
        self.elaborator
    }
}

impl<'a, H: CompilerHost> DerefMut for TypeParamScope<'_, 'a, H> {
    fn deref_mut(&mut self) -> &mut Elaborator<'a, H> {
        self.elaborator
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
        self.elaborator.annotate_ctx.trait_ctx = std::mem::take(&mut self.saved);
    }
}

impl<'a, H: CompilerHost> Elaborator<'a, H> {
    /// Enter an inherited type-param scope: the current `trait_ctx` is cloned
    /// into the saved slot but left in place, so the inner work registers
    /// additional type params on top of the parent's. A caller wanting a clean
    /// slate clears the specific fields it resets on `scope.annotate_ctx
    /// .trait_ctx` after entering; everything else stays inherited.
    pub(super) fn enter_inherited_type_param_scope(&mut self) -> TypeParamScope<'_, 'a, H> {
        let saved = self.annotate_ctx.trait_ctx.clone();
        TypeParamScope {
            elaborator: self,
            saved,
        }
    }

    /// A scope binding `impl_block`'s parameters and `Self` and nothing the
    /// caller brought: an impl block names its parameters in `impl<...>`.
    pub(super) fn enter_impl_params_scope(
        &mut self,
        impl_block: &ast::ImplBlock,
    ) -> TypeParamScope<'_, 'a, H> {
        let mut scope = self.enter_inherited_type_param_scope();
        let ctx = &mut scope.annotate_ctx.trait_ctx;
        ctx.type_params.clear();
        ctx.type_param_bounds.clear();
        ctx.assoc_type_bindings.clear();
        scope.register_impl_block_params(impl_block);
        scope
    }

    /// [`Self::enter_impl_params_scope`] with the block's associated types.
    pub(super) fn enter_impl_scope(
        &mut self,
        impl_block: &ast::ImplBlock,
    ) -> TypeParamScope<'_, 'a, H> {
        let mut scope = self.enter_impl_params_scope(impl_block);
        if impl_block.trait_type.is_some() && !impl_block.is_synthesize_request {
            let trait_name = scope.impl_block_trait_name(impl_block);
            scope.register_impl_assoc_types(impl_block, trait_name.as_ref());
        }
        scope
    }

    /// Run `body` with `names` bound to `args` and no other type parameter in
    /// scope, for resolving a type the declaration wrote against its arguments.
    ///
    /// Nothing in `struct Marked<M: Mark = Zero>` means the `Zero` a caller
    /// happens to declare. A name past `args` stays unbound, so a default
    /// reaches only the parameters to its left.
    pub(super) fn with_type_param_args<R>(
        &mut self,
        names: &[String],
        args: &[TypeId],
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let binders: IndexMap<String, BinderInScope> = names
            .iter()
            .zip(args)
            .enumerate()
            .map(|(slot, (name, &type_id))| {
                (
                    name.clone(),
                    BinderInScope::undeclared(slot as u32, type_id),
                )
            })
            .collect();
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params = binders;
        body(&mut scope)
    }

    /// [`Self::with_type_param_args`] layered on this frame instead of replacing
    /// it, for resolving a declaration's type at a site's arguments: `Self` and
    /// the enclosing parameters still answer, and a repeated name is shadowed.
    pub(super) fn with_type_params_bound<R>(
        &mut self,
        names: &[String],
        args: &[TypeId],
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let mut scope = self.enter_inherited_type_param_scope();
        let base = scope.annotate_ctx.trait_ctx.type_params.len() as u32;
        for (slot, (name, &type_id)) in names.iter().zip(args).enumerate() {
            scope.annotate_ctx.trait_ctx.type_params.insert(
                name.clone(),
                BinderInScope::undeclared(base + slot as u32, type_id),
            );
        }
        body(&mut scope)
    }

    /// Run `body` with use→def reference recording suppressed. See
    /// [`Scope::suppress_reference_recording`].
    pub(super) fn with_reference_recording_suppressed<R>(
        &mut self,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        util::replaced(
            self,
            |e| &mut e.annotate_ctx.suppress_reference_recording,
            true,
            body,
        )
        .0
    }

    /// Run `body` with `key` on the walk `stack` selects, or answer `None`
    /// where it already is: a question asked again inside itself has no answer.
    pub(super) fn unless_on_walk<K: Eq + Hash + Clone, R>(
        &mut self,
        stack: fn(&mut Scope) -> &mut IndexSet<K>,
        key: K,
        body: impl FnOnce(&mut Self) -> Option<R>,
    ) -> Option<R> {
        struct Pop<'r, 'a, H: CompilerHost, K: Eq + Hash> {
            elaborator: &'r mut Elaborator<'a, H>,
            stack: fn(&mut Scope) -> &mut IndexSet<K>,
            key: K,
        }
        impl<H: CompilerHost, K: Eq + Hash> Drop for Pop<'_, '_, H, K> {
            fn drop(&mut self) {
                (self.stack)(&mut self.elaborator.annotate_ctx).shift_remove(&self.key);
            }
        }
        if !stack(&mut self.annotate_ctx).insert(key.clone()) {
            return None;
        }
        let guard = Pop {
            elaborator: self,
            stack,
            key,
        };
        body(guard.elaborator)
    }

    /// Make `Self` stand for `binding` for the rest of this scope, every part
    /// of it at once: installing one part is the frame that leaks (#2112).
    pub(super) fn set_self_binding(&mut self, binding: SelfBinding) {
        self.annotate_ctx.trait_ctx.assoc_type_bindings.clear();
        self.annotate_ctx.trait_ctx.self_trait = binding.declaring_trait;
        self.annotate_ctx.trait_ctx.self_type = Some(binding.type_id);
    }

    /// [`Self::set_self_binding`] for the duration of `body`, restoring the
    /// frame it replaced on return (panic-safe).
    pub(super) fn with_self_binding<R>(
        &mut self,
        binding: SelfBinding,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        struct Restore<'r, 'a, H: CompilerHost> {
            elaborator: &'r mut Elaborator<'a, H>,
            saved: Option<SelfFrame>,
        }
        impl<H: CompilerHost> Drop for Restore<'_, '_, H> {
            fn drop(&mut self) {
                let saved = self.saved.take().expect("saved self frame present");
                self.elaborator.annotate_ctx.restore_self_frame(saved);
            }
        }
        let saved = self.annotate_ctx.take_self_frame();
        self.set_self_binding(binding);
        let guard = Restore {
            elaborator: self,
            saved: Some(saved),
        };
        body(guard.elaborator)
    }

    /// [`Self::with_self_binding`] where there is a binding, and `body` as it
    /// stands where there is none.
    pub(super) fn under_self_binding<R>(
        &mut self,
        binding: Option<SelfBinding>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        match binding {
            Some(binding) => self.with_self_binding(binding, body),
            None => body(self),
        }
    }

    /// Run `body` with [`Scope::resolving_home`] replaced by `module`. Unlike
    /// [`Self::with_self_binding`], `None` here is a value: it returns the walk
    /// to its own module.
    pub(super) fn with_resolving_home<R>(
        &mut self,
        module: Option<ModuleSource>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        util::replaced(self, |e| &mut e.annotate_ctx.resolving_home, module, body).0
    }

    /// The supertraits `bounds` carry, each with the trait it was reached from
    /// and the chain that reaches it. For a caller that reads the inherited
    /// bound's own types, which are written in the declaring trait's space.
    pub(super) fn inherited_bounds_of(
        &self,
        bounds: &[ast::TraitBound],
    ) -> Vec<(InheritedBound, DefId)> {
        bounds
            .iter()
            .filter(|bound| bound.names_a_trait())
            .filter_map(|bound| self.trait_decl_of(bound))
            .flat_map(|root| {
                self.supertraits_of(root)
                    .iter()
                    .map(move |inherited| (inherited.clone(), root))
            })
            .collect()
    }

    /// `bounds` expanded to include every bound's supertraits, so a declared
    /// `T: Ord` also demands `Eq`. One declaration stays one bound however
    /// spelled, so an alias never competes with its original.
    pub(super) fn elaborate_bounds(&self, bounds: &[ScopedBound]) -> Vec<ElaboratedBound> {
        // Each entry carries the declaration it merged on, so a bound that has
        // none — a `fn(..)` bound — cannot shift the ones after it.
        let mut out: Vec<(ElaboratedBound, Option<DefId>)> = Vec::with_capacity(bounds.len());
        for scoped in bounds {
            let bound = &scoped.bound;
            self.merge_bound(&mut out, bound, None, scoped.scope());
            if !bound.names_a_trait() {
                continue;
            }
            let Some(root) = self.trait_decl_of(bound) else {
                continue;
            };
            for inherited in self.supertraits_of(root) {
                // A supertrait clause is written in the declaring trait's own
                // space, not in the frame that wrote the bound reaching it.
                self.merge_bound(
                    &mut out,
                    &inherited.bound,
                    Some((root, inherited.via.clone())),
                    BoundSelf::Bounded,
                );
            }
        }
        out.into_iter().map(|(bound, _)| bound).collect()
    }

    /// Add `bound` unless the list already holds its declaration, in which case
    /// the merged bound carries what either spelling wrote — `T: Iterator +
    /// Iterator<Item = i32>` is one bound, and it carries the associated type.
    ///
    /// A `fn(..)` bound names no trait, so it has no declaration to merge on
    /// and falls back to merging on its own site: two bounds written at two
    /// sites stay two bounds, and only a bound repeated at one site merges.
    fn merge_bound(
        &self,
        out: &mut Vec<(ElaboratedBound, Option<DefId>)>,
        bound: &ast::TraitBound,
        inherited: Option<(DefId, Vec<ViaClause>)>,
        self_type: BoundSelf,
    ) {
        let entry = || ElaboratedBound {
            bound: bound.clone(),
            inherited: inherited.clone(),
            self_type,
        };
        if !bound.names_a_trait() {
            if !out
                .iter()
                .any(|(b, _)| b.bound.name == bound.name && b.bound.id == bound.id)
            {
                out.push((entry(), None));
            }
            return;
        }
        let decl = self.trait_decl_of(bound);
        // A bound that names no declaration falls back to its spelling, so an
        // erroring program still reports one bound rather than one per mention.
        // Only bounds that both write nothing are one bound. A written argument
        // names an instantiation, and a bare bound names the declared default,
        // so `Pick + Pick<String>` and `Eq<String> + Eq<StrSlice>` each ask for
        // two impls. A repeat costs a second diagnostic and nothing else.
        let mergeable = |b: &ast::TraitBound| b.type_args.is_empty() && bound.type_args.is_empty();
        let duplicate = match decl {
            Some(decl) => out
                .iter_mut()
                .find(|(b, d)| *d == Some(decl) && mergeable(&b.bound)),
            None => out
                .iter_mut()
                .find(|(b, d)| d.is_none() && b.bound.name == bound.name && mergeable(&b.bound)),
        };
        if let Some((existing, _)) = duplicate {
            if existing.bound.assoc_types.is_empty() {
                existing.bound.assoc_types.clone_from(&bound.assoc_types);
            }
            return;
        }
        out.push((entry(), decl));
    }

    /// The transitive supertraits of trait `decl`.
    fn supertraits_of(&self, decl: DefId) -> &[InheritedBound] {
        self.tysys.trait_env.supertrait_closure_declared(&decl).1
    }

    /// The type-parameter ids the enclosing generic scope owns: a slot bound to
    /// one of them is the caller forwarding its generics, not inference giving
    /// up. A `..P` pack owns its element placeholder `TypeParam { P, index }`
    /// too — the spelling `make_mapped_type_pack` gives a pack element.
    pub(super) fn scope_type_param_ids(&self) -> Vec<TypeId> {
        let mut tt = self.tysys.type_table.borrow_mut();
        self.annotate_ctx
            .trait_ctx
            .type_params
            .iter()
            .flat_map(|(name, binder)| {
                let elem = tt
                    .is_type_pack(binder.type_id)
                    .then(|| tt.make_type_param(name.clone(), binder.index));
                std::iter::once(binder.type_id).chain(elem)
            })
            .collect()
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
            // `<F: fn(...)>` binds the parameter directly to the bound's function
            // type: the bound is surface syntax for "F is exactly this
            // signature". Such params consume no `TypeParam` index slot, keeping
            // the space dense so `substitute_type_params` — keyed by
            // `TypeParam.index` — agrees with the inference cache's positions.
            let fn_bound_sig = if tp.is_pack {
                None
            } else {
                tp.bounds.iter().find_map(|b| b.fn_signature.as_ref())
            };
            let (type_id, consumed_index) = match fn_bound_sig {
                Some(sig) => (self.resolve_type(&ast::Type::Function(sig.clone())), false),
                None => (
                    self.tysys.type_table.borrow_mut().make_declared_param(
                        tp.name.clone(),
                        idx,
                        tp.is_pack,
                    ),
                    true,
                ),
            };
            let bounds = self.scoped_bounds(tp);
            self.bind_param(
                &tp.name,
                BinderInScope::declared(idx, type_id, tp.id),
                bounds,
            );
            if consumed_index {
                idx += 1;
            }
        }
        idx
    }

    /// What `Self` means at this frame: the type it stands for and the trait
    /// that declares the names projected off it. `None` in a frame binding none.
    pub(super) fn self_binding(&self) -> Option<SelfBinding> {
        Some(self.self_binding_on(self.annotate_ctx.trait_ctx.self_type?))
    }

    /// This frame's `Self`, standing on `type_id` instead. A method's
    /// declaration projects `Self::Assoc` off its receiver, under the trait the
    /// frame implements.
    pub(super) fn self_binding_on(&self, type_id: TypeId) -> SelfBinding {
        SelfBinding {
            type_id,
            declaring_trait: self.annotate_ctx.trait_ctx.self_trait,
        }
    }

    /// What `Self` means inside an `impl` block: its target, under the trait
    /// the block implements.
    pub(super) fn impl_self_binding(
        &mut self,
        impl_type: &ast::Type,
        trait_type: Option<&ast::Type>,
    ) -> SelfBinding {
        SelfBinding {
            type_id: self.resolve_type(impl_type),
            declaring_trait: trait_type.and_then(|t| self.impl_trait_decl(t)),
        }
    }

    /// Bind `name` to `binder` and to what it is bounded by, in one step.
    ///
    /// The two are one fact: a binder without its bounds dispatches on nothing,
    /// and bounds without a binder are a constraint no name reaches. A new
    /// binder is a new meaning, so the old one's bounds go with it, and a
    /// method parameter shadowing an impl's dispatches on its own traits alone.
    pub(super) fn bind_param(
        &mut self,
        name: &str,
        binder: BinderInScope,
        bounds: Vec<ScopedBound>,
    ) {
        self.annotate_ctx
            .trait_ctx
            .type_params
            .insert(name.to_string(), binder);
        self.annotate_ctx
            .trait_ctx
            .type_param_bounds
            .shift_remove(name);
        self.add_param_bounds(name, bounds);
    }

    /// File `bounds` under `name`, keeping what is already there. For a name
    /// with no binder to pair them with, and for the second half of a binding
    /// whose bounds are read after the name is bound.
    pub(super) fn add_param_bounds(&mut self, name: &str, bounds: Vec<ScopedBound>) {
        if bounds.is_empty() {
            return;
        }
        self.annotate_ctx
            .trait_ctx
            .type_param_bounds
            .entry(name.to_string())
            .or_default()
            .extend(bounds);
    }

    /// What `param` declares, pinned to the `Self` this frame binds — what the
    /// bounds' written types mean, whatever frame later reads them.
    pub(super) fn scoped_bounds(&mut self, param: &ast::GenericParam) -> Vec<ScopedBound> {
        let self_binding = self.self_binding();
        if self_binding.is_none() {
            // Every bound the parameter declares, not the trait-named ones
            // below: an `fn` bound writes types rooted at `Self` too.
            self.reject_self_in_bounds(&param.name, &param.bounds);
        }
        ScopedBound::pin_declared(param, self_binding)
    }

    /// Reject a bound writing `Self` where the frame binds none. `Self::Assoc`
    /// on a free function's parameter would go unchecked rather than mean what
    /// the parameter's own name already says.
    fn reject_self_in_bounds(&mut self, param: &str, bounds: &[ast::TraitBound]) {
        let written: Vec<Span> = bounds
            .iter()
            .filter(|bound| bound.writes_self())
            .map(|bound| bound.span)
            .collect();
        for span in written {
            let _ = self.emit(TypeError::SelfInUnboundedBound {
                param: param.to_string(),
                span,
            });
        }
    }

    /// Bind a trait's declared type parameters to the impl's concrete trait
    /// arguments: `trait Foo<T, U>` against `Foo<i32, String>` registers
    /// `T → i32` and `U → String` with their bounds. Impl-level type params must
    /// already be registered, the trait args being able to name them
    /// (`impl<X> Foo<Container<X>>`). Existing entries are left untouched.
    ///
    /// `implementing` is what the trait declared these bounds' `Self` to mean.
    /// The caller passes it rather than leaving this to read ambient state it
    /// may not have set yet.
    pub(super) fn bind_trait_type_params_from_impl(
        &mut self,
        trait_type: &ast::Type,
        implementing: SelfBinding,
    ) {
        let Some(trait_decl_type_params) = implementing
            .declaring_trait
            .and_then(|trait_| self.tysys.trait_decl_type_params_of(&trait_))
        else {
            return;
        };
        let trait_args: &[ast::Type] = match trait_type {
            ast::Type::Generic(g) => &g.args,
            _ => &[],
        };
        let supplied =
            trait_params_from_impl(&trait_decl_type_params, trait_args, Some(implementing));
        for TraitParamFromImpl {
            param, arg, bounds, ..
        } in supplied
        {
            let Some(arg) = arg else {
                continue;
            };
            if self
                .annotate_ctx
                .trait_ctx
                .type_params
                .contains_key(&param.name)
            {
                continue;
            }
            let resolved_arg = self.resolve_type(arg);
            let idx = self.annotate_ctx.trait_ctx.type_params.len() as u32;
            self.bind_param(
                &param.name,
                BinderInScope::declared(idx, resolved_arg, param.id),
                bounds,
            );
        }
    }
}
