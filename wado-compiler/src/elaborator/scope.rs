//! Transient annotate-walk scope and its RAII guards.
//!
//! Rule: scope state is mutated only through the guards and `with_*`
//! helpers in this file — every entry has exactly one panic-safe restore
//! path (WEP 2026-05-26).

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::TypeId;

use super::Elaborator;
use super::trait_env::InheritedBound;

/// Mutable trait resolution context scoped to the current resolution site.
///
/// Groups all state that changes when entering/leaving generic scopes
/// (impl blocks, trait method lookups, etc). Use
/// [`Elaborator::enter_inherited_type_param_scope`] to mutate this safely
/// with RAII restore on drop.
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
    /// Effect parameters (`<effect E>`) in scope, name → declaration
    /// `AstId`. `resolve_effects` consults this to classify a name as
    /// `EffectRef::Param` and to record its use→def edge.
    pub(super) effect_params: IndexMap<String, ast::AstId>,
}

impl TraitContext {
    /// Install the effect parameters declared in `type_params`, replacing
    /// the enclosing scope's set (restored by the caller's
    /// [`TypeParamScope`]). Must run BEFORE
    /// [`Elaborator::register_generic_params`]: eager `<F: fn() with E>`
    /// bound resolution consults this channel.
    pub(super) fn install_effect_params(&mut self, type_params: &[ast::GenericParam]) {
        self.effect_params = type_params
            .iter()
            .filter(|p| p.is_effect)
            .map(|p| (p.name.clone(), p.id))
            .collect();
    }
}

/// Per-function annotate-time scope, bundled so queries take one `&Scope`.
/// None of it may move onto the shared `TypeSystem`: `trait_ctx` is
/// per-function, `trait_check_stack` is a per-call frame stack whose
/// sharing would leak frames across module walks, and
/// `default_scope_module` is a per-call-site override.
#[derive(Default)]
pub(super) struct Scope {
    pub(super) trait_ctx: TraitContext,
    pub(super) trait_check_stack: RefCell<Vec<(TypeId, crate::defs::DefId)>>,
    /// When resolving a default-expression AST at a call site, fall back to
    /// looking up unresolved identifiers in this module's global scope —
    /// the callee's lexical scope for defaults that reference
    /// module-private items (WEP 2026-04-11).
    pub(super) default_scope_module: Option<ModuleSource>,
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

    /// Run `body` with the scope field selected by `field` set to `value`,
    /// restoring the previous value on return (panic-safe).
    fn with_scope_field<T, R>(
        &mut self,
        field: fn(&mut Scope) -> &mut T,
        value: T,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        struct Restore<'r, 'a, H: CompilerHost, T> {
            elaborator: &'r mut Elaborator<'a, H>,
            field: fn(&mut Scope) -> &mut T,
            saved: Option<T>,
        }
        impl<H: CompilerHost, T> Drop for Restore<'_, '_, H, T> {
            fn drop(&mut self) {
                *(self.field)(&mut self.elaborator.annotate_ctx) =
                    self.saved.take().expect("saved scope value present");
            }
        }
        let saved = std::mem::replace(field(&mut self.annotate_ctx), value);
        let guard = Restore {
            elaborator: self,
            field,
            saved: Some(saved),
        };
        body(guard.elaborator)
    }

    /// Run `body` with `Self` set to `self_type`.
    pub(super) fn with_self_type<R>(
        &mut self,
        self_type: TypeId,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.with_scope_field(
            |scope| &mut scope.trait_ctx.self_type,
            Some(self_type),
            body,
        )
    }

    /// Run `body` with [`Scope::default_scope_module`] replaced by
    /// `module`. Unlike [`Self::with_self_type_if_known`], `None` here is
    /// a value: it clears the fallback.
    pub(super) fn with_default_scope_module<R>(
        &mut self,
        module: Option<ModuleSource>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.with_scope_field(|scope| &mut scope.default_scope_module, module, body)
    }

    /// Expand a written bound list to include every bound's supertraits, so a
    /// declared `T: Ord` also demands `Eq`. For the sites that *check* a bound;
    /// what a parameter is known to satisfy is elaborated on read instead, by
    /// [`super::tysys::TypeSystem::bound_implies`]. One declaration stays one
    /// bound however spelled, so an alias never competes with its original.
    pub(super) fn elaborate_bounds(&self, bounds: &[ast::TraitBound]) -> Vec<ast::TraitBound> {
        self.elaborate_bounds_with(bounds, &IndexMap::default())
    }

    /// [`Self::elaborate_bounds`] for bounds carrying no reference site of their
    /// own. `known` maps a bound's id to the declaration it means, answered where
    /// the bound was first read: a projection's bounds are rebuilt here with
    /// fresh ids the table cannot answer for, and without `known` the dedup would
    /// fall back to the spelling and collapse two same-named traits.
    pub(super) fn elaborate_bounds_with(
        &self,
        bounds: &[ast::TraitBound],
        known: &IndexMap<crate::ast::AstId, crate::name::FqTraitName>,
    ) -> Vec<ast::TraitBound> {
        // Each entry carries the declaration it merged on, so a bound that has
        // none — a `fn(..)` bound — cannot shift the ones after it.
        let mut out: Vec<(ast::TraitBound, Option<crate::defs::DefId>)> =
            Vec::with_capacity(bounds.len());
        for bound in bounds {
            self.merge_bound(&mut out, bound, known);
            if bound.fn_signature.is_some() {
                continue;
            }
            for inherited in self.supertraits_of_bound(bound, known) {
                self.merge_bound(&mut out, &inherited.bound, known);
            }
        }
        out.into_iter().map(|(bound, _)| bound).collect()
    }

    /// Add `bound` unless the list already holds its declaration, in which case
    /// the constrained spelling wins — `T: Iterator + Iterator<Item = i32>` is
    /// one bound, and it is the one carrying the associated type.
    ///
    /// A `fn(..)` bound names no trait, so it has no declaration to merge on
    /// and falls back to merging on its own site: two bounds written at two
    /// sites stay two bounds, and only a bound repeated at one site merges.
    fn merge_bound(
        &self,
        out: &mut Vec<(ast::TraitBound, Option<crate::defs::DefId>)>,
        bound: &ast::TraitBound,
        known: &IndexMap<crate::ast::AstId, crate::name::FqTraitName>,
    ) {
        if bound.fn_signature.is_some() {
            if !out
                .iter()
                .any(|(b, _)| b.name == bound.name && b.id == bound.id)
            {
                out.push((bound.clone(), None));
            }
            return;
        }
        let decl = self.bound_decl(bound, known);
        // A bound that names no declaration falls back to its spelling, so an
        // erroring program still reports one bound rather than one per mention.
        let duplicate = match decl {
            Some(decl) => out.iter_mut().find(|(_, d)| *d == Some(decl)),
            None => out
                .iter_mut()
                .find(|(b, d)| d.is_none() && b.name == bound.name),
        };
        if let Some((existing, _)) = duplicate {
            if existing.assoc_types.is_empty() && !bound.assoc_types.is_empty() {
                *existing = bound.clone();
            }
            return;
        }
        out.push((bound.clone(), decl));
    }

    /// The declaration a bound names: `known` first, then the bound's own site.
    fn bound_decl(
        &self,
        bound: &ast::TraitBound,
        known: &IndexMap<crate::ast::AstId, crate::name::FqTraitName>,
    ) -> Option<crate::defs::DefId> {
        known
            .get(&bound.id)
            .and_then(crate::name::FqTraitName::canonical)
            .or_else(|| self.trait_decl_at(bound.id, &bound.name))
    }

    /// The transitive supertraits of the trait `bound` names, as bounds.
    ///
    /// Answered from the bound's own reference site: two modules may declare
    /// the same name, and expanding by spelling picks whichever the by-name
    /// index holds — for the loser, an empty closure, so a supertrait's methods
    /// silently vanish.
    fn supertraits_of_bound(
        &self,
        bound: &ast::TraitBound,
        known: &IndexMap<crate::ast::AstId, crate::name::FqTraitName>,
    ) -> Vec<InheritedBound> {
        self.bound_decl(bound, known)
            .map(|decl| self.tysys.trait_env.supertrait_closure(&decl).to_vec())
            .unwrap_or_default()
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
            let (type_id, consumed_index) = if tp.is_pack {
                (
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_pack(tp.name.clone(), idx),
                    true,
                )
            } else if let Some(sig) = fn_bound_sig {
                (self.resolve_type(&ast::Type::Function(sig.clone())), false)
            } else {
                (
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_param(tp.name.clone(), idx),
                    true,
                )
            };
            self.annotate_ctx
                .trait_ctx
                .type_params
                .insert(tp.name.clone(), (idx, type_id));
            self.annotate_ctx
                .trait_ctx
                .type_param_decls
                .insert(tp.name.clone(), tp.id);
            // Filter out `fn`/`fn mut` bounds before recording (they're already
            // realised in the bound type itself); only "real" trait bounds need
            // remembering for method lookup.
            let real_bounds = tp.real_bounds();
            if !real_bounds.is_empty() {
                self.annotate_ctx
                    .trait_ctx
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
    /// arguments: `trait Foo<T, U>` against `Foo<i32, String>` registers
    /// `T → i32` and `U → String` with their bounds. Impl-level type params must
    /// already be registered, the trait args being able to name them
    /// (`impl<X> Foo<Container<X>>`). Existing entries are left untouched.
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
            if self
                .annotate_ctx
                .trait_ctx
                .type_params
                .contains_key(&tp.name)
            {
                continue;
            }
            let Some(arg_ast) = trait_args.get(i) else {
                continue;
            };
            let resolved_arg = self.resolve_type(arg_ast);
            let idx = self.annotate_ctx.trait_ctx.type_params.len() as u32;
            self.annotate_ctx
                .trait_ctx
                .type_params
                .insert(tp.name.clone(), (idx, resolved_arg));
            if !tp.bounds.is_empty() {
                self.annotate_ctx
                    .trait_ctx
                    .type_param_bounds
                    .entry(tp.name.clone())
                    .or_default()
                    .extend(tp.bounds.clone());
            }
        }
    }
}
