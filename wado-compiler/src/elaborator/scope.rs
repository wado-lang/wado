//! Transient annotate-walk scope and its RAII guards.
//!
//! Rule: scope state is mutated only through the guards and `with_*`
//! helpers in this file — every entry has exactly one panic-safe restore
//! path (WEP 2026-07-10).

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::TypeId;

use super::Elaborator;

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
    pub(super) trait_check_stack: RefCell<Vec<(TypeId, String)>>,
    /// When resolving a default-expression AST at a call site, fall back to
    /// looking up unresolved identifiers in this module's global scope —
    /// the callee's lexical scope for defaults that reference
    /// module-private items (WEP 2026-04-11).
    pub(super) default_scope_module: Option<ModuleSource>,
}

/// RAII guard that restores `Elaborator::trait_ctx` to its saved value on drop.
///
/// Implements `Deref<Target = Elaborator>` so it can be used as a transparent
/// elaborator handle inside the scope. Restoration is panic-safe: even if the
/// scope body panics, drop still runs and the parent context is reinstated.
///
/// Use [`Elaborator::enter_inherited_type_param_scope`] to enter a new scope.
/// It preserves the current `trait_ctx` so the child scope can register new
/// entries on top of the parent's. Callers that want a clean slate for a
/// specific field (matching the legacy `mem::take` pattern) should clear that
/// field on `scope.annotate_ctx.trait_ctx` after entering.
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
    /// Enter an inherited type-param scope. The current `trait_ctx` is cloned
    /// into the saved slot, but left in place so the inner work can register
    /// additional type params on top of what the parent already had. The
    /// original context is restored when the returned guard is dropped.
    ///
    /// Callers that want a clean slate (matching the legacy
    /// `mem::take(&mut self.annotate_ctx.trait_ctx.type_params)` pattern) should clear the
    /// specific fields they want to reset on `scope.annotate_ctx.trait_ctx` after entering
    /// the scope — only the fields they touch need to be cleared, all others
    /// are inherited from the parent scope.
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
    /// declared `T: Ord` also demands `Eq`. Only for the sites that *check* a
    /// bound: what a type parameter is known to satisfy is elaborated on read
    /// ([`super::tysys::TypeSystem::bound_implies`]), never on registration —
    /// there are too many registration sites to keep in step.
    /// `fn(...)` bounds name no trait and pass through untouched.
    pub(super) fn elaborate_bounds(&self, bounds: &[ast::TraitBound]) -> Vec<ast::TraitBound> {
        let mut elaborated = Vec::with_capacity(bounds.len());
        for bound in bounds {
            super::trait_env::push_unique_bound(&mut elaborated, bound);
            if bound.fn_signature.is_some() {
                continue;
            }
            for inherited in self
                .tysys
                .supertraits_of(&self.type_lookup(), &bound.name)
                .to_vec()
            {
                super::trait_env::push_unique_bound(&mut elaborated, &inherited);
            }
        }
        elaborated
    }

    /// [`Self::elaborate_bounds`] over bare trait names.
    pub(super) fn elaborate_bound_names(&self, names: &[String]) -> Vec<String> {
        let mut elaborated: Vec<String> = Vec::with_capacity(names.len());
        for name in names {
            if !elaborated.contains(name) {
                elaborated.push(name.clone());
            }
            for inherited in self
                .tysys
                .supertraits_of(&self.type_lookup(), name)
                .to_vec()
            {
                if !elaborated.contains(&inherited.name) {
                    elaborated.push(inherited.name);
                }
            }
        }
        elaborated
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
            let real_bounds: Vec<ast::TraitBound> = tp
                .bounds
                .iter()
                .filter(|b| b.fn_signature.is_none())
                .cloned()
                .collect();
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
