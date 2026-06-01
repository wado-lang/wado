//! Reify — AST + [`super::sem::ModuleSemantics`] → [`crate::tir::TirModule`].
//!
//! Introduced by Stage 5 of [`wep-2026-05-26-elaborator-rearchitecture.md`].
//! The reify pass is the mechanical half of the annotate/reify split:
//! every TIR-shaping decision is already recorded on `ModuleSemantics`
//! during `annotate_bodies`; this walker reads those annotations and emits
//! the corresponding TIR nodes. It never re-runs type inference, name
//! resolution, or method dispatch.
//!
//! # Surface
//!
//! `reify_module(module, tysys, sem, …) → TirModule` mirrors
//! [`super::Elaborator::resolve_module`]'s per-Item dispatch shape. Each
//! `Item::*` arm calls a `reify_*` helper; decl-only items (`Enum`,
//! `Flags`, `Newtype`, `Variant`, `Effect`, `Resource`, `Struct`) read
//! decl-interned types from `TypeSystem.all_*` and produce TIR without
//! consulting `TypeAnnotations`; function / impl-method / test / global
//! bodies build a fresh [`super::types::FunctionContext`] and walk the
//! AST via `reify_block` / `reify_stmt` / `reify_expr` / `reify_pattern`.
//!
//! # What reify reads
//!
//! - `ModuleSemantics.types.expression_types[id]` → `TirExpr::type_id`
//! - `ModuleSemantics.types.method_dispatch[id]` → dispatch target +
//!   `self_kind` + `is_ref_impl`
//! - `ModuleSemantics.types.coercions[id]` → coercion wrapper to emit
//!   around the raw expression
//! - `ModuleSemantics.types.desugars[id]` → which expansion path to take
//!   (assert / matches / for-of / while / compound-assign / comparison
//!   chain / `IndexMut` method call / newtype-from collapse)
//! - `ModuleSemantics.types.generic_instantiations[id]` → `type_args`
//!   for call / struct / variant constructions
//! - `ModuleSemantics.types.closure_captures[id]` → closure capture list
//!   and `__ref_*` materialisation
//! - `ModuleSemantics.types.assert_captures[id]` → assert slot map
//! - `ModuleSemantics.types.for_of_iterator[id]` → for-of iterator
//!   dispatch target
//! - `ModuleSemantics.bindings.references` / `bindings.local_symbols` →
//!   identifier resolution
//! - `ModuleSemantics.imports.*` → name lookups
//! - `ModuleSemantics.decls.*` → function return types, generic
//!   parameter tables, anonymous-struct registry
//!
//! # What reify mutates
//!
//! Reify takes `&mut TypeSystem` because monomorphic struct / variant
//! instances reached for the first time at reify time must intern through
//! the shared [`crate::tir::TypeTable`]. Reify does **not** mutate
//! `TraitEnv` or any impl tables — those are read-only inputs.
//!
//! # `FunctionContext` walk-order invariant
//!
//! Reify maintains its own [`super::types::FunctionContext`] per function
//! body. Local indices, capture indices, and synthetic-local counters
//! (`next_assert_id`, `next_loop_id`, the `__ref_*` ordering) must match
//! what `annotate_bodies` produced. The contract is documented at
//! [`wep-2026-05-26-elaborator-rearchitecture.md`] §`Design notes (Stage
//! 5)` →`Gap 7: per-function local-frame walk-order invariant`. The unit
//! test contract: for every function `f`, the `Vec<TirLocal>` annotate
//! emitted equals the `Vec<TirLocal>` reify emits.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{self, AstId, Item, Module};
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::symbol::SymbolTable;
use crate::tir::{
    self as tir, TirBlock, TirEnum, TirEnumCase, TirExpr, TirFlags, TirFlagsMember, TirFunction,
    TirGlobal, TirModule, TirNewtype, TirPattern, TirStmt, TirStruct, TirTest, TirVariantDecl,
    TypeId,
};

use super::sem::ModuleSemantics;
use super::types::{FunctionContext, TypeLookup};
use super::tysys::TypeSystem;

/// Generate the `ann_*` annotation accessors on [`Reify`]. Each expands to
/// a method that builds the canonical `SymbolKey` for `id` in the current
/// module, walks `self.tuple_overlay_stack` innermost-first looking for that
/// key, then falls back to `self.sem.types.<map>`, returning a clone. See
/// the accessor doc comment on the `impl` block for why this exists.
macro_rules! reify_annotation_accessors {
    ($($name:ident => $map:ident : $val:ty),+ $(,)?) => {
        $(
            fn $name(&self, id: crate::ast::AstId) -> Option<$val> {
                // Annotation maps are keyed by the canonical `SymbolKey`
                // (`(ModuleSource, AstId)`); reify swaps
                // `current_module_source` while walking foreign AST so the
                // key always names the module the node actually came from.
                let key = crate::symbol::SymbolKey::new(
                    self.current_module_source.clone(),
                    id,
                );
                for overlay in self.tuple_overlay_stack.iter().rev() {
                    if let Some(v) = overlay.$map.get(&key) {
                        return Some(v.clone());
                    }
                }
                self.sem.types.$map.get(&key).cloned()
            }
        )+
    };
    // `base { … }`: decl/signature facts that are recorded once per decl and
    // are NOT part of the per-element tuple-for-of overlay (param/return types,
    // type params, effect-op signatures). Same canonical keying, no overlay
    // walk — reify reads them straight from `sem.types`.
    (base { $($name:ident => $map:ident : $val:ty),+ $(,)? }) => {
        $(
            fn $name(&self, id: crate::ast::AstId) -> Option<$val> {
                let key = crate::symbol::SymbolKey::new(
                    self.current_module_source.clone(),
                    id,
                );
                self.sem.types.$map.get(&key).cloned()
            }
        )+
    };
}

/// Per-module reify pass. One instance per loaded module the batch driver
/// emits TIR for.
///
/// Construction is via [`Reify::new`]; the driver hands in the shared
/// `TypeSystem` (cloned by shallow Rc/Arc copy), the read-only
/// `ModuleSemantics` for this module, and the per-compile context the
/// elaborator already threads through.
///
/// `#[allow(dead_code)]` on the struct + every method is intentional
/// until [`crate::elaborator::orchestration`] wires the
/// `annotate_bodies → reify_modules` pipeline split (the second half of
/// Stage 5). The skeleton lands first so the membership contract is
/// One reify-side power-assert capture slot. Independent from
/// [`super::assert::Capture`] so the two walks don't share state.
#[allow(dead_code)]
pub(super) struct ReifyAssertSlot {
    pub(super) ast_id: AstId,
    /// `__v0`, `__v1`, … — the local the panic template references.
    pub(super) name: String,
    pub(super) label: String,
    /// `false` when the sub-expression evaporated during reify and no
    /// `let __vK = …;` was emitted; the template skips the slot.
    pub(super) emitted: bool,
    pub(super) local_index: Option<u32>,
    pub(super) type_id: Option<crate::tir::TypeId>,
}

pub(super) struct ReifyAssertCaptureContext {
    pub(super) slots: Vec<ReifyAssertSlot>,
    pub(super) ast_id_to_slot: IndexMap<AstId, usize>,
    /// Guard so the `reify_expr` hook doesn't re-fire on the same
    /// `AstId` during its own recursive reify call.
    pub(super) in_progress: IndexSet<AstId>,
    pub(super) emitted_lets: Vec<TirStmt>,
}

pub(crate) struct Reify<'a, H: CompilerHost> {
    /// Pipeline-wide type knowledge. `&mut` only because reify may
    /// intern new monomorphic instances; the trait/impl tables are
    /// treated as read-only per the WEP `Reify surface` contract.
    pub(crate) tysys: TypeSystem,
    /// Per-module semantic facts produced by `annotate_bodies`. Read
    /// only — reify never mutates the recorded decisions.
    pub(crate) sem: &'a ModuleSemantics,
    /// All modules' semantics, keyed by source. Used to swap `sem` to a
    /// callee module when reifying a default-argument expression that
    /// resolves in the callee's lexical scope (it may reference items
    /// private to the callee module).
    pub(crate) all_module_semantics: &'a IndexMap<ModuleSource, ModuleSemantics>,
    /// Symbol table from analyzer (cross-module).
    #[allow(dead_code)]
    pub(crate) symbols: &'a SymbolTable,
    /// All loaded modules. Used by cross-module lookups (e.g. resolving
    /// the AST of a function referenced by a `FunctionRef`).
    #[allow(dead_code)]
    pub(crate) loaded_modules: &'a IndexMap<ModuleSource, Module>,
    /// Diagnostics logger.
    pub(crate) logger: &'a Logger<'a, H>,
    /// Source module currently being reified.
    pub(crate) current_module_source: ModuleSource,
    /// Items of the current module, set before per-Item dispatch.
    pub(crate) current_module_items: &'a [Item],
    /// `ModuleSource` interner. Shared with annotate so cross-pass
    /// references resolve to the same `ModuleSource` identity.
    #[allow(dead_code)]
    pub(crate) interner: Rc<RefCell<ModuleSourceInterner>>,
    /// Kiln invocation redirects. Consulted by `use`-resolution paths
    /// the same way annotate consults them.
    #[allow(dead_code)]
    pub(crate) invocations: Rc<crate::kiln::InvocationIndex>,
    /// Entry module, used for cross-module import dedup.
    #[allow(dead_code)]
    pub(crate) entry_module_source: ModuleSource,
    /// Type-parameter names in scope for the function/method body
    /// currently being reified (impl params first, then method-level
    /// params, matching the index layout reify builds in
    /// `reify_method` / `reify_function`). Empty outside a body walk.
    /// `resolve_type` consults this so a turbofish type argument naming
    /// an enclosing type param (`v.serialize::<S>(s)` inside a generic
    /// method) resolves to its `TypeParam` slot instead of `unknown`.
    pub(crate) current_type_param_names: Vec<String>,
    /// Names of the effect parameters (`<effect E>`) in scope for the
    /// function / method currently being reified. `reify_effects` and
    /// `apply_function_type_effects` consult this so an effect name that is a
    /// param resolves to [`crate::tir::EffectRef::Param`] rather than a
    /// `Concrete` effect — matching `Elaborator::resolve_effects`. Without
    /// it a `fn(...) with E` parameter type would carry `Concrete { E }`,
    /// which fails to unify with the enclosing function's recorded
    /// `Param { E }` declared effect at indirect-call effect checks.
    pub(crate) current_effect_param_names: Vec<String>,
    /// Active per-element annotation overlays for the tuple `for-of`(s)
    /// currently being unrolled, innermost last. While reifying element
    /// `i` of a tuple for-of, that element's [`ElementOverlay`] sits on
    /// top; the annotation accessors (`ann_*`) consult the stack from the
    /// top down before falling back to `sem.types`. A nested inner for-of
    /// pushes its own overlay above the outer one, so inner-body nodes
    /// shadow correctly while outer-body nodes fall through to the outer
    /// overlay. See [`Self::reify_tuple_for_of`].
    pub(crate) tuple_overlay_stack: Vec<super::sem::types::ElementOverlay>,
    /// Per-`ForOfStmt` visit counter. Annotate records one overlay set per
    /// *instantiation* of a tuple for-of in walk order; reify increments
    /// this each time it reifies the same `for_of.id` so it consumes the
    /// matching instantiation (a nested inner for-of is instantiated once
    /// per outer element). See [`Self::reify_tuple_for_of`].
    pub(crate) tuple_overlay_visits: IndexMap<crate::symbol::SymbolKey, usize>,
}

#[allow(dead_code)]
impl<'a, H: CompilerHost> Reify<'a, H> {
    /// Construct a per-module `Reify` for the orchestration driver.
    /// The `tysys` clone is the shallow Rc/Arc copy
    /// [`TypeSystem`] supports; per-module state (`sem`,
    /// `current_module_*`) is borrowed from
    /// [`crate::elaborator::orchestration::AnnotateState`] for the
    /// duration of the reify walk.
    ///
    /// `current_module_source` / `current_module_items` are
    /// placeholders at construction time — the driver overwrites them
    /// inside [`Self::reify_module`]. Keeping them on the struct
    /// matches the elaborator's shape (avoids threading them through
    /// every method signature) and keeps the walk-order invariant
    /// (Gap 7) tractable.
    pub(crate) fn new(
        tysys: TypeSystem,
        sem: &'a ModuleSemantics,
        all_module_semantics: &'a IndexMap<ModuleSource, ModuleSemantics>,
        symbols: &'a SymbolTable,
        loaded_modules: &'a IndexMap<ModuleSource, Module>,
        logger: &'a Logger<'a, H>,
        entry_module_source: ModuleSource,
        interner: Rc<RefCell<ModuleSourceInterner>>,
        invocations: Rc<crate::kiln::InvocationIndex>,
    ) -> Self {
        Self {
            tysys,
            sem,
            all_module_semantics,
            symbols,
            loaded_modules,
            logger,
            current_module_source: ModuleSource::entry_point_uninitialized(),
            current_module_items: &[],
            interner,
            invocations,
            entry_module_source,
            current_type_param_names: Vec::new(),
            current_effect_param_names: Vec::new(),
            tuple_overlay_stack: Vec::new(),
            tuple_overlay_visits: IndexMap::default(),
        }
    }

    // Per-element annotation accessors (`ann_*`) that honour active tuple
    // `for-of` overlays.
    //
    // A tuple for-of's body is a single source sub-tree resolved once per
    // element by annotate; to keep each element's distinct facts, annotate
    // moved them out of the base `sem.types` maps into per-element
    // `ElementOverlay`s (and *truncated* the base maps — so for body
    // `AstId`s the fact lives ONLY in the overlay). Every reify read of one
    // of those maps must therefore go through the matching `ann_*`
    // accessor, which walks `tuple_overlay_stack` innermost-first and falls
    // back to `sem.types`. Outside a tuple for-of the stack is empty and
    // the accessor is just the base-map lookup. The macro keeps the 14-map
    // list in one place, mirroring `TypeAnnotations::split_off_overlay`.
    reify_annotation_accessors! {
        ann_expression_types => expression_types: crate::tir::TypeId,
        ann_method_dispatch => method_dispatch: super::sem::types::MethodDispatch,
        ann_coercions => coercions: super::sem::types::CoercionChoice,
        ann_desugars => desugars: super::sem::types::DesugarKind,
        ann_generic_instantiations => generic_instantiations: super::sem::types::GenericInstantiation,
        ann_closure_captures => closure_captures: super::sem::types::ClosureCaptureInfo,
        ann_call_param_types => call_param_types: Vec<crate::tir::TypeId>,
        ann_assert_captures => assert_captures: super::sem::types::AssertCaptureInfo,
        ann_for_of_iterator => for_of_iterator: super::sem::types::ForOfIteratorInfo,
        ann_operator_dispatch => operator_dispatch: super::sem::types::OperatorDispatch,
        ann_static_method_dispatch => static_method_dispatch: super::sem::types::StaticMethodDispatch,
        ann_sequence_coercions => sequence_coercions: super::sem::types::SequenceCoercionFacts,
        ann_key_value_coercions => key_value_coercions: super::sem::types::KeyValueCoercionFacts,
        ann_index_assign_dispatch => index_assign_dispatch: super::sem::types::OperatorDispatch,
    }

    /// Read the recorded type of a local binding (keyed by the binding's
    /// def `AstId`). Unlike the `ann_*` accessors above, `local_types` is not
    /// part of the per-element tuple-for-of overlay — but reify only consults
    /// it for *annotated* `let` types, which are written in source and so are
    /// element-invariant (a for-of binds a value, not a type), making the
    /// base map's last-write value correct for every unrolled element.
    fn ann_local_type(&self, id: crate::ast::AstId) -> Option<crate::tir::TypeId> {
        let key = crate::symbol::SymbolKey::new(self.current_module_source.clone(), id);
        self.sem.types.local_types.get(&key).copied()
    }

    // Decl/signature facts the combined walk records once per decl (the
    // single source of truth), read straight from `sem.types` with no overlay
    // walk:
    //   - `method_impl_type_params`: the impl-type-param scheme
    //     `resolve_method` recorded per impl-method `AstId`.
    //   - `fn_param_types` / `fn_return_types`: a function/method's resolved
    //     param types (declaration order, receiver included) and
    //     post-async-erasure return type.
    //   - `effect_ops`: an effect/resource decl's resolved op signatures.
    //   - `decl_type_params`: TIR type params per decl (function, method,
    //     struct, variant), defaults resolved with the scope alive.
    reify_annotation_accessors! {
        base {
            ann_method_impl_type_params => method_impl_type_params: Vec<crate::tir::TirTypeParam>,
            ann_fn_param_types => fn_param_types: Vec<crate::tir::TypeId>,
            ann_fn_return_type => fn_return_types: crate::tir::TypeId,
            ann_effect_ops => effect_ops: Vec<crate::tir::TirEffectOp>,
            ann_decl_type_params => decl_type_params: Vec<crate::tir::TirTypeParam>,
        }
    }

    /// Build a [`TypeLookup`] view over the current module's import
    /// context and the shared `all_*` tables. Used by `reify_*` helpers
    /// that need to resolve AST `Type` nodes (e.g. type-param defaults,
    /// resource method param/return types) the same way the elaborator
    /// did during annotate — but without the elaborator's
    /// `record_type_name_reference` side-effect (use→def edges were
    /// already recorded by annotate and live on
    /// [`ModuleSemantics::bindings`]).
    fn type_lookup(&self) -> TypeLookup<'_> {
        TypeLookup {
            current_module_source: &self.current_module_source,
            imported_type_sources: &self.sem.imports.imported_type_sources,
            import_original_names: &self.sem.imports.import_original_names,
            all_newtypes: &self.tysys.all_newtypes,
            all_struct_fields: &self.tysys.all_struct_fields,
            all_variant_cases: &self.tysys.all_variant_cases,
            all_enum_cases: &self.tysys.all_enum_cases,
            all_flags_cases: &self.tysys.all_flags_cases,
            all_resource_types: &self.tysys.all_resource_types,
            all_generic_newtypes: &self.tysys.all_generic_newtypes,
            local_struct_fields: &self.sem.decls.local_struct_fields,
            local_newtypes: &self.sem.decls.local_newtypes,
            local_enum_cases: &self.sem.decls.local_enum_cases,
            local_flags_cases: &self.sem.decls.local_flags_cases,
            local_generic_newtypes: &self.sem.decls.local_generic_newtypes,
            local_variant_cases: &self.sem.decls.local_variant_cases,
        }
    }

    /// Resolve an effect-name list into [`crate::tir::EffectRef`]s
    /// for a function signature. Mirrors
    /// [`super::Elaborator::resolve_effects`] (elaborator.rs:948+)
    /// without the use→def recording side-effect (annotate already
    /// recorded the edges).
    fn reify_effects(&self, effects: &[String]) -> Vec<crate::tir::EffectRef> {
        effects
            .iter()
            .map(|name| {
                // Effect params in scope (`<effect E>`) become `Param`, matching
                // `Elaborator::resolve_effects`; otherwise they would resolve to
                // a `Concrete` effect and fail to unify with the recorded
                // `Param` declared effect at effect checks.
                if self.current_effect_param_names.iter().any(|p| p == name) {
                    crate::tir::EffectRef::Param { name: name.clone() }
                } else if let Some(source) = self.sem.imports.effect_sources.get(name).cloned() {
                    let canonical = self
                        .symbols
                        .lookup_in_module(&source, name)
                        .map(|sym| sym.defined_at.module.clone())
                        .unwrap_or_else(|| source.clone());
                    crate::tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: canonical,
                    }
                } else {
                    let canonical = self
                        .symbols
                        .lookup(name)
                        .map(|sym| sym.defined_at.module.clone())
                        .unwrap_or_else(|| self.current_module_source.clone());
                    crate::tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: canonical,
                    }
                }
            })
            .collect()
    }

    /// Resolve an AST [`ast::Type`] to a [`TypeId`] without recording
    /// any use→def edge. Reify uses this for type-level resolutions
    /// (type-param defaults, resource method params, …) — annotate
    /// already recorded the edges during its body walk.
    ///
    /// Delegates to the existing
    /// [`super::Elaborator::resolve_type_static`] helper, which is
    /// host-agnostic and operates over the [`TypeLookup`] view above.
    fn resolve_type(&mut self, ty: &ast::Type) -> TypeId {
        let lookup = self.type_lookup();
        // Resolve within the current body's type-parameter scope so a
        // turbofish argument that names an enclosing type param resolves
        // to its `TypeParam` slot. Outside a body walk the scope is empty,
        // so this is identical to the scope-free path.
        let resolved = super::Elaborator::<H>::resolve_type_static_with_params(
            ty,
            &mut self.tysys.type_table.borrow_mut(),
            &lookup,
            &self.current_type_param_names,
        );
        self.apply_function_type_effects(ty, resolved)
    }

    /// Re-intern a resolved `fn(...) with E` type carrying its effects: the
    /// shared static resolver has no effect-resolution context and leaves
    /// `effects` empty, so a `fn`-typed parameter loses its `with` clause.
    /// `check_effects` then can't see that, e.g., `f: fn() with Stdout`
    /// requires `Stdout` at an indirect call site. Resolves effects through
    /// the same [`Self::reify_effects`] used for declared effects (so the
    /// `EffectRef`s stay canonically consistent across the module). Handles a
    /// bare `Function` and one behind `&` / `&mut`.
    fn apply_function_type_effects(&self, ty: &ast::Type, resolved: TypeId) -> TypeId {
        use crate::tir::ResolvedType;
        match ty {
            ast::Type::Reference(inner) => {
                let pointee = match self.tysys.type_table.borrow().get(resolved) {
                    ResolvedType::Ref(p) => *p,
                    _ => return resolved,
                };
                let fixed = self.apply_function_type_effects(inner, pointee);
                if fixed == pointee {
                    resolved
                } else {
                    self.tysys.type_table.borrow_mut().make_ref(fixed)
                }
            }
            ast::Type::MutReference(inner) => {
                let pointee = match self.tysys.type_table.borrow().get(resolved) {
                    ResolvedType::MutRef(p) => *p,
                    _ => return resolved,
                };
                let fixed = self.apply_function_type_effects(inner, pointee);
                if fixed == pointee {
                    resolved
                } else {
                    self.tysys.type_table.borrow_mut().make_mut_ref(fixed)
                }
            }
            ast::Type::Function(ft) if !ft.effects.is_empty() => {
                let effects = self.reify_effects(&ft.effects);
                let rebuilt = match self.tysys.type_table.borrow().get(resolved) {
                    ResolvedType::Function {
                        is_mut,
                        params,
                        return_type,
                        stores,
                        ..
                    } => ResolvedType::Function {
                        is_mut: *is_mut,
                        params: params.clone(),
                        return_type: *return_type,
                        effects,
                        stores: stores.clone(),
                    },
                    _ => return resolved,
                };
                self.tysys.type_table.borrow_mut().intern(rebuilt)
            }
            _ => resolved,
        }
    }

    /// Reify one module: emit a [`TirModule`] from the AST + the
    /// `ModuleSemantics` `annotate_bodies` populated.
    ///
    /// The flow mirrors [`super::Elaborator::resolve_module`] item by
    /// item; only the per-Item dispatch shape is reproduced here, the
    /// body of each branch is delegated to a `reify_*` helper.
    pub(crate) fn reify_module(
        &mut self,
        module: &'a Module,
        module_source: ModuleSource,
    ) -> Result<TirModule, Bail> {
        self.current_module_source = module_source.clone();
        self.current_module_items = &module.items;

        let mut tir_module = TirModule::new(module_source);

        for item in &module.items {
            match item {
                Item::Function(func) => {
                    if let Some(tir_func) = self.reify_function(func) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Struct(struct_decl) => {
                    tir_module.add_struct(self.reify_struct(struct_decl));
                }
                Item::Impl(impl_block) => {
                    for tir_func in self.reify_impl(impl_block) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Trait(_) => {
                    // Trait declarations don't lower to TIR; the elaborator
                    // already registered the signature on `TraitEnv`.
                }
                Item::Variant(variant_decl) => {
                    tir_module
                        .variants
                        .push(self.reify_variant_decl(variant_decl));
                }
                Item::Test(test_decl) => {
                    let test_index = tir_module.tests.len();
                    let module_is_todo = module.has_todo();
                    if let Some((tir_func, tir_test)) =
                        self.reify_test_decl(test_decl, test_index, module_is_todo)
                    {
                        tir_module.add_function(tir_func);
                        tir_module.tests.push(tir_test);
                    }
                }
                Item::Global(global_decl) => {
                    if let Some(tir_global) = self.reify_global(global_decl) {
                        tir_module.globals.push(tir_global);
                    }
                }
                Item::Enum(enum_decl) => {
                    tir_module.add_enum(self.reify_enum(enum_decl));
                }
                Item::Flags(flags_decl) => {
                    if let Some(tir_flags) = self.reify_flags(flags_decl) {
                        tir_module.add_flags(tir_flags);
                    }
                }
                Item::Newtype(newtype_decl) => {
                    if let Some(tir_newtype) = self.reify_newtype(newtype_decl) {
                        tir_module.add_newtype(tir_newtype);
                    }
                }
                Item::Interface(effect_decl) => {
                    tir_module.add_effect(self.reify_effect_decl(effect_decl));
                }
                Item::Resource(resource_decl) => {
                    tir_module.add_resource(self.reify_resource_decl(resource_decl));
                }
                _ => {}
            }
        }

        // Share the type table via Rc::clone so downstream phases see
        // the same arena reify just interned into.
        tir_module.type_table = Rc::clone(&self.tysys.type_table);

        if let Some(data) = module.data_section() {
            tir_module = tir_module.with_data_section(Some(data.to_string()));
        }

        // Anonymous structs synthesised during body resolution by
        // `annotate_bodies` live on `sem.decls.pending_anonymous_structs`.
        // Reify clones them into the emitted module rather than draining,
        // because `sem` is `&` here.
        for anon_struct in &self.sem.decls.pending_anonymous_structs {
            tir_module.add_struct(anon_struct.clone());
        }

        // Stage 5 / Gap 12: forward the per-module synthesis
        // requests and default-method synthesis output annotate
        // recorded on `ModuleDecls`. Same shape the existing
        // combined walk pushes during the `Item::Impl` arm; the
        // recording side already mirrors both writes so reify
        // produces the same `TirModule` content.
        for req in &self.sem.decls.pending_synthesis_requests {
            tir_module.synthesis_requests.push(req.clone());
        }
        for default_method in &self.sem.decls.pending_default_methods {
            tir_module.add_function(default_method.clone());
        }

        tir_module.wasm_module = module.wasm_module().map(String::from);

        self.logger.ok_or_bail(tir_module)
    }

    // ─────────────────────────────────────────────────────────────────
    // Decl-only items: read from `tysys.all_*` and produce TIR without
    // consulting `TypeAnnotations`.
    // ─────────────────────────────────────────────────────────────────

    /// Reify an `enum E { … }` declaration. Pure projection from the
    /// AST shape; cases keep their declared index.
    fn reify_enum(&self, enum_decl: &ast::EnumDecl) -> TirEnum {
        TirEnum {
            name: enum_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: enum_decl.is_pub,
            type_params: Vec::new(),
            monomorph_info: None,
            cases: enum_decl
                .cases
                .iter()
                .enumerate()
                .map(|(i, case)| TirEnumCase {
                    name: case.name.clone(),
                    index: i as u32,
                    span: case.span,
                })
                .collect(),
            span: enum_decl.span,
        }
    }

    /// Reify a `flags F { … }` declaration. The `TypeId` is the one
    /// `annotate_decls` interned via `make_flags`; reify reads it from
    /// `tysys.all_flags_cases`.
    fn reify_flags(&self, flags_decl: &ast::FlagsDecl) -> Option<TirFlags> {
        let info = self
            .tysys
            .all_flags_cases
            .get(&self.current_module_source)?
            .get(&flags_decl.name)?;
        Some(TirFlags {
            name: flags_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: flags_decl.is_pub,
            type_id: info.type_id,
            members: flags_decl
                .flags
                .iter()
                .enumerate()
                .map(|(i, m)| TirFlagsMember {
                    name: m.name.clone(),
                    bitmask: 1u32 << i,
                    span: m.span,
                })
                .collect(),
            span: flags_decl.span,
        })
    }

    /// Reify a `newtype N = T;` declaration (concrete only). Generic
    /// newtypes are instantiated on demand by `make_newtype_instance`;
    /// the concrete decl shape is what lands in TIR.
    fn reify_newtype(&self, newtype_decl: &ast::Newtype) -> Option<TirNewtype> {
        if !newtype_decl.type_params.is_empty() {
            // Generic newtypes have no concrete TIR decl emitted at the
            // module level; they materialise per-instantiation.
            return None;
        }
        let type_id = *self
            .tysys
            .all_newtypes
            .get(&self.current_module_source)?
            .get(&newtype_decl.name)?;
        Some(TirNewtype {
            name: newtype_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: newtype_decl.is_pub,
            type_id,
            span: newtype_decl.span,
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with sub-declaration walks but no function bodies.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a `struct S { … }`. Field types come from
    /// `tysys.all_struct_fields`; field-default expressions and
    /// type-param defaults are read from `ModuleSemantics`.
    fn reify_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        // Field types are decl-resolved during annotate and live in
        // `tysys.all_struct_fields`. Snapshot the per-field types into
        // an owned `Vec` so the borrow on `self.tysys` ends here —
        // the field-default reify below mutably borrows `self`.
        let mut field_types: Vec<TypeId> = {
            let field_info = self
                .tysys
                .all_struct_fields
                .get(&self.current_module_source)
                .and_then(|m| m.get(&struct_decl.name));
            (0..struct_decl.fields.len())
                .map(|i| {
                    field_info
                        .and_then(|info| info.fields.get(i).map(|(_, t, _)| *t))
                        .unwrap_or(crate::tir::TypeTable::UNKNOWN)
                })
                .collect()
        };

        // The static decl-field pass (`resolve_type_static` → `TypeLookup`
        // without `loaded_modules`) cannot follow `pub use` re-export chains,
        // so a field typed by a re-exported decl (e.g. `Mark = u64` re-exported
        // from `wasi:clocks`) lands `UNKNOWN`. Reify's own [`Self::resolve_type`]
        // carries `loaded_modules`, so re-resolving the field's AST annotation
        // recovers the real type. Production masks the same `UNKNOWN` by
        // re-resolving through the instance resolver at emission time.
        for (index, field) in struct_decl.fields.iter().enumerate() {
            if field_types[index] == crate::tir::TypeTable::UNKNOWN {
                field_types[index] = self.resolve_type(&field.ty);
            }
        }

        // Field-default expressions resolve in a per-struct
        // `FunctionContext` keyed `struct:<name>` (no self, no other
        // fields in scope), matching `Elaborator::resolve_struct` at
        // item.rs:461 byte-for-byte so the synthesized purity check
        // and reify see identical TIR.
        let mut field_ctx = FunctionContext::new(
            crate::tir::TypeTable::UNIT,
            format!("struct:{}", struct_decl.name),
        );

        let mut fields = Vec::with_capacity(struct_decl.fields.len());
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = field_types[index];

            let serde_rename = field.attrs.iter().find_map(|a| {
                if a.name == "serde" {
                    a.kv_value("rename").map(str::to_string)
                } else {
                    None
                }
            });

            let default_expr: Option<Box<TirExpr>> = field.default.as_ref().map(|default_ast| {
                Box::new(self.reify_expr(default_ast, &mut field_ctx, Some(type_id)))
            });

            let serde_default = field.default.is_some()
                || field
                    .attrs
                    .iter()
                    .any(|a| a.name == "serde" && a.has_arg("default"));

            fields.push(crate::tir::TirField {
                name: field.name.clone(),
                is_pub: field.is_pub,
                type_id,
                index: index as u32,
                span: field.span,
                is_hidden: field.attrs.iter().any(|a| a.name == "hidden"),
                serde_rename,
                serde_default,
                default_expr,
            });
        }

        // Single source of truth: the combined walk projected these type
        // params with each default resolved while the decl's type-param scope
        // was alive; read them back rather than re-resolving the defaults.
        let type_params = self
            .ann_decl_type_params(struct_decl.id)
            .expect("resolve_struct records the type params for every struct reify emits");

        let serde_rename_all = struct_decl.attrs.iter().find_map(|a| {
            if a.name == "serde" {
                a.kv_value("rename_all").map(str::to_string)
            } else {
                None
            }
        });

        TirStruct {
            name: struct_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: struct_decl.is_pub,
            type_params,
            monomorph_info: None,
            fields,
            span: struct_decl.span,
            serde_rename_all,
        }
    }

    /// Reify a `variant V<T> { … }` declaration. Cases' payload types
    /// come from `tysys.all_variant_cases`; the type-param table is
    /// projected from the AST.
    fn reify_variant_decl(&mut self, variant_decl: &ast::VariantDecl) -> TirVariantDecl {
        let case_info = self
            .tysys
            .all_variant_cases
            .get(&self.current_module_source)
            .and_then(|m| m.get(&variant_decl.name));

        let cases: Vec<tir::TirVariantCase> = variant_decl
            .cases
            .iter()
            .enumerate()
            .map(|(index, case)| {
                let payload = case_info
                    .and_then(|info| info.cases.get(index).map(|c| c.payload))
                    .unwrap_or(crate::tir::TypeTable::UNIT);
                tir::TirVariantCase {
                    name: case.name.clone(),
                    index: index as u32,
                    payload,
                    span: case.span,
                }
            })
            .collect();

        // Single source of truth: read the type params the combined walk
        // projected (defaults resolved with the decl's scope alive) rather
        // than re-resolving the defaults here.
        let type_params = self
            .ann_decl_type_params(variant_decl.id)
            .expect("resolve_variant_decl records the type params for every variant reify emits");

        TirVariantDecl {
            name: variant_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            is_pub: variant_decl.is_pub,
            type_params,
            cases,
            span: variant_decl.span,
        }
    }

    /// Reify an `interface E { … }` declaration. Effects have no
    /// `Self` type — `&self` / `&mut self` on an effect method is a
    /// surface error annotate already diagnosed.
    fn reify_effect_decl(&mut self, decl: &ast::InterfaceDecl) -> tir::TirEffect {
        // Single source of truth: the combined walk resolved the op
        // signatures with the decl's type-param / `Self` scope in place and
        // recorded them; reify reads them back rather than re-resolving.
        let operations = self
            .ann_effect_ops(decl.id)
            .expect("resolve_effect_decl records op signatures for every effect reify emits");
        tir::TirEffect {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
        }
    }

    /// Reify a `resource R<T> { … }` declaration. Resource methods take
    /// a synthesised `self` parameter (`&Self` or `&mut Self`) at index
    /// 0; for generic resources `Self = GenericResource<…>` with the
    /// decl's own `TypeParam`s as type args. The op signatures are read
    /// from the facts the combined walk recorded.
    fn reify_resource_decl(&mut self, decl: &ast::ResourceDecl) -> tir::TirResource {
        let operations = self
            .ann_effect_ops(decl.id)
            .expect("resolve_resource_decl records op signatures for every resource reify emits");
        tir::TirResource {
            name: decl.name.clone(),
            is_pub: decl.is_pub,
            operations,
            span: decl.span,
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with function bodies — the bulk of reify_*.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a free function. Builds a fresh `FunctionContext`, walks
    /// params + body, and assembles `TirFunction`. All inference
    /// decisions are read from `sem.types`; reify only emits TIR.
    ///
    /// Stage 5 staging: the function shape is implemented (return type
    /// from `sem.decls.function_return_types`, parameters added in
    /// declaration order to pin the walk-order invariant, body
    /// delegated to [`Self::reify_block`]). Generic-bound checking,
    /// effect-param scoping, the `<F: fn(…)>` bound realisation pass,
    /// and `extract_*` attribute helpers are left to a follow-up — they
    /// are pure projections that don't depend on the body walk, but
    /// would duplicate `Elaborator` helpers and balloon this file.
    fn reify_function(&mut self, func: &ast::Function) -> Option<TirFunction> {
        // Single source of truth: read the (post-async-erasure) return type
        // `resolve_function` resolved, rather than re-reading the fragile
        // name-keyed `function_return_types` map (shared with call sites and
        // overwritable by later registrations).
        let return_type = self
            .ann_fn_return_type(func.id)
            .expect("resolve_function records the return type for every function reify emits");

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(return_type);
        }

        // Real type params only (effect params and `<F: fn(...)>` bounds
        // are excluded), so the positional indices stay dense and match
        // the emitted `type_params` and monomorph's substitution keys.
        let type_param_names: Vec<String> = func
            .type_params
            .iter()
            .filter(|p| !p.is_effect && !p.bounds.iter().any(|b| b.fn_signature.is_some()))
            .map(|p| p.name.clone())
            .collect();

        // Effect params (`<effect E>`) drive `Param` effect resolution in
        // function-type params; publish them for the body walk.
        let effect_param_names: Vec<String> = func
            .type_params
            .iter()
            .filter(|p| p.is_effect)
            .map(|p| p.name.clone())
            .collect();
        let saved_effect_param_names =
            std::mem::replace(&mut self.current_effect_param_names, effect_param_names);

        // Publish the body's type-param scope (see `reify_method`).
        let saved_type_param_names =
            std::mem::replace(&mut self.current_type_param_names, type_param_names);

        // Single source of truth: read the resolved param types
        // `resolve_function` recorded (in `func.params` order, with `<F: fn>`
        // bounds already realised), rather than re-resolving each here.
        let param_types = self
            .ann_fn_param_types(func.id)
            .expect("resolve_function records param types for every function reify emits");
        let mut params = Vec::with_capacity(func.params.len());
        for (p_idx, param) in func.params.iter().enumerate() {
            let type_id = param_types[p_idx];
            let default_expr = param
                .default
                .as_ref()
                .map(|default_ast| Box::new(self.reify_expr(default_ast, &mut ctx, Some(type_id))));
            let index = ctx.add_local(param.name.clone(), type_id, param.is_mut, Some(param.id));
            params.push(tir::TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                default_expr,
                span: param.span,
            });
        }

        let body = func
            .body
            .as_ref()
            .map(|b| self.reify_block(b, &mut ctx, None));

        self.current_type_param_names = saved_type_param_names;
        self.current_effect_param_names = saved_effect_param_names;

        // Single source of truth: read the TIR type params `resolve_function`
        // projected (effect / `fn`-bound params filtered, dense indices,
        // defaults resolved with the type-param scope alive), rather than
        // re-projecting them here after the scope is torn down.
        let type_params = self
            .ann_decl_type_params(func.id)
            .expect("resolve_function records the type params for every function reify emits");

        Some(TirFunction {
            module_source: ModuleSource::default(),
            name: func.name.clone(),
            is_pub: func.is_pub,
            is_export: func.is_export,
            is_async: func.is_async,
            type_params,
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params,
            return_type,
            // Async functions erase `return_type` to `()`; the real
            // (declared) return travels via `task return` and is recorded
            // by annotate in `function_task_returns`. Resource-store
            // inference (effect_check) walks `task_return_type`, so it
            // must carry the real type, not the erased unit. (Methods are
            // not recorded there and fall back to `return_type`.)
            task_return_type: if func.is_async {
                Some(
                    self.sem
                        .types
                        .function_task_returns
                        .get(&crate::symbol::SymbolKey::new(
                            self.current_module_source.clone(),
                            func.id,
                        ))
                        .copied()
                        .unwrap_or(return_type),
                )
            } else {
                None
            },
            effects: self
                .sem
                .types
                .function_effects
                .get(&crate::symbol::SymbolKey::new(
                    self.current_module_source.clone(),
                    func.id,
                ))
                .cloned()
                .unwrap_or_else(|| self.reify_effects(&func.effects)),
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: extract_is_ambient_attr(&func.attrs),
            inline_hint: extract_inline_hint_attr(&func.attrs),
            compiler_item: crate::elaborator::item::extract_compiler_item(
                &func.attrs,
                func.span,
                self.logger,
            ),
            export_name: extract_export_name_attr(&func.attrs),
            allocator_tag: extract_allocator_tag_attr(&func.attrs),
            kind: tir::FunctionKind::Regular,
            return_abi: tir::ReturnAbi::Single,
        })
    }

    /// Reify every method on an `impl` block. Reads the impl-block
    /// resolution facts annotate recorded
    /// (`sem.types.impl_facts[impl_block.id]`, Gap 12) and threads
    /// them into [`Self::reify_method`] per AST `Function`.
    ///
    /// Synthesis-request impls (`impl Trait for Type;`) emit no
    /// methods — the request itself lives on
    /// `sem.decls.pending_synthesis_requests` and is forwarded to
    /// `TirModule::synthesis_requests` at [`Self::reify_module`].
    ///
    /// Default-method synthesis is also out of scope here: the
    /// synthesised `TirFunction`s live on
    /// `sem.decls.pending_default_methods` and forward through the
    /// per-module path. This keeps the responsibility split clean
    /// (per-impl-block code in `reify_impl`, per-module aggregation
    /// in `reify_module`).
    fn reify_impl(&mut self, impl_block: &ast::ImplBlock) -> Vec<TirFunction> {
        if impl_block.is_synthesize_request {
            return Vec::new();
        }
        let impl_key =
            crate::symbol::SymbolKey::new(self.current_module_source.clone(), impl_block.id);
        let Some(facts) = self.sem.types.impl_facts.get(&impl_key).cloned() else {
            // Annotate did not record facts — the impl block was
            // diagnosed by annotate (e.g. unknown trait reference)
            // and skipped. Reify follows by emitting no methods.
            return Vec::new();
        };

        impl_block
            .methods
            .iter()
            .filter_map(|method| self.reify_method(method, &facts, &impl_block.ty))
            .collect()
    }

    /// Reify a single method inside an `impl` block. The method's
    /// body walk shares the structure with [`Self::reify_function`];
    /// the difference is that the receiver (`&self` / `&mut self`)
    /// is synthesised from the recorded [`super::sem::types::ImplFacts::self_type`]
    /// (no re-resolution of the impl target), and the resulting
    /// [`TirFunction`] carries the `method_info` /
    /// `impl_type_params` reify reads from the same recorded facts.
    fn reify_method(
        &mut self,
        func: &ast::Function,
        facts: &super::sem::types::ImplFacts,
        impl_self_ty: &ast::Type,
    ) -> Option<TirFunction> {
        use crate::ast::SelfKind;
        use crate::name::{LocalMethodName, MethodName};
        use crate::tir::TypeTable;

        // Single source of truth: the impl-type-param scheme is computed once
        // by `Elaborator::resolve_method` and recorded; reify reads it. reify
        // runs only for the current module's explicitly-written methods in the
        // same `build_tir_from_state` pass that recorded them (stdlib is
        // rehydrated from the snapshot's already-reified TIR), so the fact is
        // always present — a missing entry is a contract violation, not a
        // fallback case.
        let impl_type_params: Vec<crate::tir::TirTypeParam> =
            self.ann_method_impl_type_params(func.id).expect(
                "resolve_method records the impl-type-param scheme for every \
                 impl method reify emits",
            );

        // Type-param scope for resolving the method's own param/return
        // types. Every impl-self-type arg occupies its positional slot —
        // including concrete / known-named args (`String` in
        // `TreeMap<String, V>`) — exactly as the elaborator's
        // `resolve_method` registers them (item.rs). They are NOT excluded:
        // doing so was a reify-only divergence that disagreed with the
        // battle-tested original path; the elaborator treats such an arg as a
        // positional param and monomorph substitutes it back to the concrete
        // type by identity. Method-level params continue after the impl param
        // count, matching `resolve_method`'s `next_idx = impl_type_params.len()`
        // — the same base the monomorphizer uses
        // (`impl_type_params.len() + param.index` in
        // `func_inst::instantiate_function`).
        let mut type_param_names: Vec<String> = Vec::new();
        for p in &impl_type_params {
            let idx = p.index as usize;
            if type_param_names.len() <= idx {
                type_param_names.resize(idx + 1, String::new());
            }
            type_param_names[idx] = p.name.clone();
        }
        let mut next_idx = impl_type_params.len();
        for p in &func.type_params {
            // Skip `<F: fn(...)>` bounds: like the elaborator they are
            // realised eagerly to the bound's function type (built into
            // `fn_bound_map` below) and must not consume a positional
            // type-param slot, or the real method params shift index.
            if p.is_effect
                || p.bounds.iter().any(|b| b.fn_signature.is_some())
                || type_param_names.iter().any(|n| n == &p.name)
            {
                continue;
            }
            if type_param_names.len() <= next_idx {
                type_param_names.resize(next_idx + 1, String::new());
            }
            type_param_names[next_idx] = p.name.clone();
            next_idx += 1;
        }

        // Method-level effect params (`<effect E>`) drive `Param` effect
        // resolution in function-type params; publish them for the method
        // body walk.
        let effect_param_names: Vec<String> = func
            .type_params
            .iter()
            .filter(|p| p.is_effect)
            .map(|p| p.name.clone())
            .collect();
        let saved_effect_param_names =
            std::mem::replace(&mut self.current_effect_param_names, effect_param_names);

        // Derive the mangler's base-struct-name input from the
        // resolved `Self` type. The mangler wants the bare name
        // (`Box`, not `Box<T>`); the type table's
        // `type_name(self_type)` returns the mangled form, so
        // truncate at the first `<`.
        //
        // A variadic tuple impl (`impl<..T> Trait for [..T]`) is the
        // exception: `type_name` renders the tuple self type in bracket
        // notation (`[..T]`, no `<`), so the truncation would mangle the
        // method as `[..T]^Trait::method`. Production instead uses the
        // builtin tuple's `name` field — `Tuple` — for both the method
        // name and `method_info` (item.rs:660 / method_call.rs:724), so
        // the call site's `monomorph_info.generic_name` (`Tuple^…`) and
        // the monomorphizer's tuple-variadic instantiation path
        // (func_inst.rs:888, gated on `struct_name == TUPLE_TYPE_NAME`)
        // both find the template. Match that here.
        // A reference impl (`impl<T: Bound> Trait for &T` /
        // `impl<…> Trait for &Array<T>`) mangles to base struct `&` / `&mut`,
        // independent of the inner type — production's `get_type_name`
        // (module.rs:581) returns exactly `"&"` / `"&mut"` for any reference
        // target, and the monomorphizer / template synthesis look up the
        // blanket template by that bare name (`&^Inspect::inspect`), keyed off
        // the inner type via `impl_type_args`. Deriving the name from the
        // *resolved* self type instead would mangle `&T` → `&T^…`, a name the
        // monomorphizer never queries, leaving every `&T`-blanket method call
        // (e.g. `&i32^Inspect::inspect` from `{x:?}` on a reference) unresolved.
        let base_struct_name = match impl_self_ty {
            ast::Type::Reference(_) => "&".to_string(),
            ast::Type::MutReference(_) => "&mut".to_string(),
            _ if self.tysys.type_table.borrow().is_tuple(facts.self_type) => {
                TypeTable::TUPLE_TYPE_NAME.to_string()
            }
            _ => {
                let struct_name_for_mangle: String =
                    self.tysys.type_table.borrow().type_name(facts.self_type);
                struct_name_for_mangle
                    .split('<')
                    .next()
                    .unwrap_or(&struct_name_for_mangle)
                    .to_string()
            }
        };
        let mangled_name = MethodName::format_local(
            &base_struct_name,
            facts.trait_name_mangled.as_deref(),
            &func.name,
        );
        let method_info = {
            let mut info = LocalMethodName::new(
                base_struct_name,
                facts.trait_name_mangled.clone(),
                func.name.clone(),
            );
            info.is_ref_impl = facts.is_ref_impl;
            // Carry the impl's trait type args (`impl Future<i32> for …`
            // → `[i32]`). The effect-dispatch synthesis keys its handler
            // index on `(struct, effect_module, base_trait, trait_type_args)`
            // (effect_dispatch.rs:2984); without the args a generic-effect
            // handler is keyed `Future<>` and the `Future<i32>` binding
            // finds no `DispatchPlan`.
            info.trait_type_args = facts.trait_type_args.clone();
            if let Some((module, base)) = facts.trait_canonical.clone() {
                info.base_trait_module = Some(module);
                info.base_trait_name = Some(base);
            }
            info
        };

        // Single source of truth: read the return type `resolve_method`
        // resolved, rather than re-resolving the return annotation.
        let return_type = self
            .ann_fn_return_type(func.id)
            .expect("resolve_method records the return type for every impl method reify emits");

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        ctx.in_handler_method = facts.is_handler_method;
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(return_type);
        }

        // Publish the body's type-param scope so turbofish args in the
        // body (`v.serialize::<S>(s)`) resolve against it. Restored before
        // returning so decl-level resolution stays scope-free.
        let saved_type_param_names =
            std::mem::replace(&mut self.current_type_param_names, type_param_names.clone());

        // Single source of truth: read the resolved param types
        // `resolve_method` recorded (in `func.params` order, receiver
        // included), rather than re-resolving each here.
        let param_types = self
            .ann_fn_param_types(func.id)
            .expect("resolve_method records param types for every impl method reify emits");
        let mut params = Vec::with_capacity(func.params.len());
        for (p_idx, p) in func.params.iter().enumerate() {
            let type_id = param_types[p_idx];
            let name = if matches!(p.self_kind, SelfKind::None) {
                p.name.clone()
            } else {
                "self".to_string()
            };
            let default_expr = p
                .default
                .as_ref()
                .map(|d| Box::new(self.reify_expr(d, &mut ctx, Some(type_id))));
            let local_index = ctx.add_local(name.clone(), type_id, p.is_mut, Some(p.id));
            params.push(crate::tir::TirParam {
                name,
                type_id,
                local_index,
                is_mut: p.is_mut,
                default_expr,
                span: p.span,
            });
        }

        let body = func
            .body
            .as_ref()
            .map(|b| self.reify_block(b, &mut ctx, None));

        self.current_type_param_names = saved_type_param_names;
        self.current_effect_param_names = saved_effect_param_names;

        // Single source of truth: read the method-level type params
        // `resolve_method` projected (effect / `fn`-bound params filtered,
        // dense indices, defaults resolved with the type-param scope alive),
        // rather than re-projecting them here after the scope is torn down.
        let type_params = self.ann_decl_type_params(func.id).expect(
            "resolve_method records the method type params for every impl method reify emits",
        );

        Some(TirFunction {
            module_source: ModuleSource::default(),
            name: mangled_name,
            is_pub: func.is_pub,
            is_export: false,
            is_async: func.is_async,
            type_params,
            impl_type_params,
            monomorph_info: None,
            method_info: Some(method_info),
            params,
            return_type,
            // Async functions erase `return_type` to `()`; the real
            // (declared) return travels via `task return` and is recorded
            // by annotate in `function_task_returns`. Resource-store
            // inference (effect_check) walks `task_return_type`, so it
            // must carry the real type, not the erased unit. (Methods are
            // not recorded there and fall back to `return_type`.)
            task_return_type: if func.is_async {
                Some(
                    self.sem
                        .types
                        .function_task_returns
                        .get(&crate::symbol::SymbolKey::new(
                            self.current_module_source.clone(),
                            func.id,
                        ))
                        .copied()
                        .unwrap_or(return_type),
                )
            } else {
                None
            },
            effects: self
                .sem
                .types
                .function_effects
                .get(&crate::symbol::SymbolKey::new(
                    self.current_module_source.clone(),
                    func.id,
                ))
                .cloned()
                .unwrap_or_else(|| self.reify_effects(&func.effects)),
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: extract_is_ambient_attr(&func.attrs),
            inline_hint: extract_inline_hint_attr(&func.attrs),
            compiler_item: crate::elaborator::item::extract_compiler_item(
                &func.attrs,
                func.span,
                self.logger,
            ),
            export_name: extract_export_name_attr(&func.attrs),
            allocator_tag: extract_allocator_tag_attr(&func.attrs),
            kind: crate::tir::FunctionKind::Regular,
            return_abi: crate::tir::ReturnAbi::Single,
        })
    }

    /// Reify a `test "…" { … }` block. Returns the synthesised
    /// `TirFunction` plus the `TirTest` metadata. Mirrors
    /// `Elaborator::resolve_test_decl` (item.rs:1233+): the function
    /// name encodes `test_index` + attributes (`expect_trap`, `TODO`,
    /// `timeout_ms`); the body reifies into a unit-returning
    /// no-parameter function.
    fn reify_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
        module_is_todo: bool,
    ) -> Option<(TirFunction, TirTest)> {
        use crate::tir::{FunctionKind, InlineHint, ReturnAbi, TypeTable};

        let expect_trap = test_decl.attributes.iter().any(|a| a.name == "expect_trap");
        let is_todo = module_is_todo || test_decl.attributes.iter().any(|a| a.name == "TODO");
        let timeout_ms = test_decl.attributes.iter().find_map(|a| {
            if a.name == "timeout_ms" {
                a.args
                    .first()
                    .and_then(|arg| arg.as_str().parse::<u64>().ok())
            } else {
                None
            }
        });

        let prefix = match (is_todo, expect_trap, timeout_ms) {
            (true, _, Some(ms)) => format!("__test_todo_tm{ms}"),
            (true, _, None) => "__test_todo".to_string(),
            (_, true, Some(ms)) => format!("__test_trap_tm{ms}"),
            (_, true, None) => "__test_trap".to_string(),
            (_, _, Some(ms)) => format!("__test_tm{ms}"),
            (_, _, None) => "__test".to_string(),
        };
        let function_name = match &test_decl.name {
            // Use the shared ASCII-only snake conversion: non-ASCII letters
            // must collapse to `_` so the segment downgrades losslessly into
            // a Component Model kebab-case export name. A Unicode-aware
            // `is_alphanumeric` here would leak multibyte letters into the
            // export name and crash Wasm validation (matches item.rs).
            Some(name) => {
                let snake_name = crate::name::test_name_to_snake(name);
                format!("{prefix}_{test_index}_{snake_name}")
            }
            None => format!("{prefix}_{test_index}"),
        };

        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());
        let body = self.reify_block(&test_decl.body, &mut ctx, None);

        let tir_func = TirFunction {
            module_source: ModuleSource::default(),
            name: function_name.clone(),
            is_pub: false,
            is_export: false,
            is_async: false,
            type_params: vec![],
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params: vec![],
            return_type,
            task_return_type: None,
            effects: vec![],
            stores: vec![],
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: InlineHint::Auto,
            compiler_item: None,
            export_name: None,
            allocator_tag: None,
            kind: FunctionKind::Regular,
            return_abi: ReturnAbi::default(),
        };

        let tir_test = TirTest {
            name: test_decl.name.clone(),
            function_name,
            line: test_decl.span.line,
            span: test_decl.span,
            expect_trap,
            is_todo,
            timeout_ms,
        };

        Some((tir_func, tir_test))
    }

    /// Reify a `global g: T = expr;` declaration. The declared type
    /// was already resolved by `annotate_decls` and lives on
    /// `sem.decls.current_module_globals`; reify reads it back and
    /// walks the initializer through a minimal `FunctionContext`.
    /// `is_nullable` / `lazy_init` are populated by the lower phase
    /// (kept `false` here, matching `Elaborator::resolve_global`).
    fn reify_global(&mut self, global_decl: &ast::GlobalDecl) -> Option<TirGlobal> {
        let ty = self
            .sem
            .decls
            .current_module_globals
            .get(&global_decl.name)
            .map(|(t, _)| *t)
            .unwrap_or_else(|| self.resolve_type(&global_decl.ty));

        let mut ctx = FunctionContext::new(ty, format!("global:{}", global_decl.name));
        let initializer = self.reify_expr(&global_decl.initializer, &mut ctx, Some(ty));

        Some(TirGlobal {
            name: global_decl.name.clone(),
            ty,
            initializer,
            mutable: global_decl.mutable,
            wado_mutable: global_decl.mutable,
            is_pub: global_decl.is_pub,
            module_source: self.current_module_source.clone(),
            span: global_decl.span,
            is_nullable: false,
            lazy_init: false,
            locals: ctx.locals.clone(),
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Body walks: expressions, statements, blocks, patterns.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a block expression — walks each statement in order so
    /// `FunctionContext::locals` matches what annotate produced.
    pub(super) fn reify_block(
        &mut self,
        block: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirBlock {
        ctx.enter_scope();
        let len = block.stmts.len();
        let mut stmts = Vec::new();
        for (i, s) in block.stmts.iter().enumerate() {
            // Propagate expected type to the last expression/statement
            // for coercion, mirroring `Elaborator::resolve_block`
            // (stmt.rs:40–66): a trailing `Expr` / `If` / `Match` /
            // `LabeledBlock` in value position keeps its result flowing
            // out as the block's value rather than being dropped at
            // statement position.
            if expected_type.is_some() && i == len - 1 {
                if let ast::Stmt::Expr(expr_stmt) = s {
                    let expr = self.reify_expr(&expr_stmt.expr, ctx, expected_type);
                    stmts.push(TirStmt::new(
                        crate::tir::TirStmtKind::Expr(expr),
                        expr_stmt.span,
                    ));
                    continue;
                }
                if let ast::Stmt::If(if_stmt) = s {
                    stmts.extend(self.reify_if_stmt_with_expected(if_stmt, ctx, expected_type));
                    continue;
                }
                if let ast::Stmt::Match(match_expr) = s {
                    let recorded = self
                        .ann_expression_types(match_expr.id)
                        .or(expected_type)
                        .unwrap_or(crate::tir::TypeTable::UNKNOWN);
                    let tir = self.reify_match_expr(match_expr, ctx, expected_type, recorded);
                    stmts.push(TirStmt::new(
                        crate::tir::TirStmtKind::Expr(tir),
                        match_expr.span,
                    ));
                    continue;
                }
                if let ast::Stmt::LabeledBlock(labeled_block) = s {
                    ctx.active_labels.push(labeled_block.label.clone());
                    let block = self.reify_block(&labeled_block.block, ctx, expected_type);
                    ctx.active_labels.pop();
                    stmts.push(TirStmt::new(
                        crate::tir::TirStmtKind::LabeledBlock {
                            label: labeled_block.label.clone(),
                            block,
                        },
                        labeled_block.span,
                    ));
                    continue;
                }
            }
            stmts.extend(self.reify_stmt(s, ctx));
        }
        ctx.exit_scope();
        TirBlock::new(stmts, block.span)
    }

    /// Reify a statement. Dispatches on `Stmt::*`; `Let` adds a local
    /// (preserving walk-order), `For` / `While` / `Assert` consult
    /// `sem.types.desugars` to pick the right expansion path.
    pub(super) fn reify_stmt(
        &mut self,
        stmt: &ast::Stmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::TirStmtKind;
        match stmt {
            ast::Stmt::Expr(expr_stmt) => {
                let expr = self.reify_expr(&expr_stmt.expr, ctx, None);
                vec![TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span)]
            }
            ast::Stmt::Return(ret_stmt) => {
                let value = ret_stmt
                    .value
                    .as_ref()
                    .map(|e| self.reify_expr(e, ctx, Some(ctx.return_type)));
                vec![TirStmt::new(TirStmtKind::Return { value }, ret_stmt.span)]
            }
            ast::Stmt::TaskReturn(tr_stmt) => {
                let expected = ctx.task_return_type;
                let value = self.reify_expr(&tr_stmt.value, ctx, expected);
                vec![TirStmt::new(
                    TirStmtKind::TaskReturn { value },
                    tr_stmt.span,
                )]
            }
            ast::Stmt::Break(break_stmt) => {
                // Resolve `break label: value` against the target block's
                // expected type so a `null` / bare literal value coerces to
                // the block's result type (e.g. `Option<i32>`) rather than
                // reaching WIR as an unresolved `Option<UNKNOWN>` / nullref.
                let break_expected = break_stmt.label.as_ref().and_then(|label| {
                    ctx.labeled_block_targets
                        .iter()
                        .rev()
                        .find(|t| &t.label == label)
                        .and_then(|t| t.expected_type)
                });
                vec![TirStmt::new(
                    TirStmtKind::Break {
                        label: break_stmt.label.clone(),
                        value: break_stmt
                            .value
                            .as_ref()
                            .map(|e| self.reify_expr(e, ctx, break_expected)),
                    },
                    break_stmt.span,
                )]
            }
            ast::Stmt::Continue(continue_stmt) => {
                // Inside a C-style `for`, `continue` must break to the
                // body label so the `update` expression runs before the
                // next iteration; only while/loop bodies use a plain
                // `Continue`. Mirror `Elaborator::resolve_continue`
                // (stmt.rs:251), keyed off `ctx.for_continue_labels`.
                let stmt_kind = if let Some(body_label) = ctx.for_continue_labels.last() {
                    TirStmtKind::Break {
                        label: Some(body_label.clone()),
                        value: None,
                    }
                } else {
                    TirStmtKind::Continue
                };
                vec![TirStmt::new(stmt_kind, continue_stmt.span)]
            }
            ast::Stmt::Let(let_stmt) => vec![self.reify_let(let_stmt, ctx)],
            ast::Stmt::If(if_stmt) => self.reify_if_stmt(if_stmt, ctx),
            ast::Stmt::Match(match_expr) => {
                // Stmt-position match — `Elaborator::resolve_stmt`
                // pins `expected_type = Some(Unit)` and records the
                // result type explicitly (stmt.rs ≈84–105). Reify
                // mirrors: reify the expression at Unit, then wrap as
                // an `Expr` stmt. The reified expression's
                // `type_id` will already be `Unit` (annotate's
                // `expression_types` records the stmt-position type),
                // so the WIR builder drops each arm body's value.
                let tir = self.reify_match_expr(
                    match_expr,
                    ctx,
                    Some(crate::tir::TypeTable::UNIT),
                    crate::tir::TypeTable::UNIT,
                );
                vec![TirStmt::new(TirStmtKind::Expr(tir), match_expr.span)]
            }
            ast::Stmt::Loop(loop_stmt) => {
                // `loop { … }` — direct lowering. The
                // `for_continue_labels` save/restore mirrors
                // `Elaborator::resolve_loop` (stmt.rs:2092–2101).
                let saved = std::mem::take(&mut ctx.for_continue_labels);
                let body = self.reify_block(&loop_stmt.body, ctx, None);
                ctx.for_continue_labels = saved;
                vec![TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)]
            }
            ast::Stmt::LabeledBlock(labeled_block) => {
                // `LABEL: { … }` stmt — mirrors
                // `Elaborator::resolve_labeled_block` (stmt.rs:116+).
                // Push the label onto `active_labels` so a nested
                // `break LABEL` lowers against this frame, walk the
                // inner block, pop. The block result is dropped at
                // stmt position, so no `expected_type` propagates.
                ctx.active_labels.push(labeled_block.label.clone());
                let block = self.reify_block(&labeled_block.block, ctx, None);
                ctx.active_labels.pop();
                vec![TirStmt::new(
                    TirStmtKind::LabeledBlock {
                        label: labeled_block.label.clone(),
                        block,
                    },
                    labeled_block.span,
                )]
            }
            ast::Stmt::While(w) => self.reify_while(w, ctx),
            ast::Stmt::For(f) => self.reify_for(f, ctx),
            ast::Stmt::Assert(assert_stmt) => self.reify_assert(assert_stmt, ctx),
            ast::Stmt::ForOf(for_of) => self.reify_for_of(for_of, ctx),
        }
    }

    /// Reify `let pat[: T] = expr;`. Currently handles the common
    /// `Ident` / `MutIdent` / `Wildcard` patterns; destructuring
    /// (`Tuple` / `Struct` / `Variant`) defers to the dispatcher's
    /// `todo!`.
    fn reify_let(&mut self, let_stmt: &ast::LetStmt, ctx: &mut FunctionContext) -> TirStmt {
        use crate::tir::{TirStmtKind, TypeTable};
        // Uninitialised `let x: T;` — the parser guarantees `ty`
        // is present. The WIR builder zero-initialises the slot;
        // reify emits a Unit placeholder as the `value` and the
        // `type_id` field carries the user-declared type. Refutable
        // patterns in this position are rejected at annotate; the
        // recovery path emits an Expr-Unit placeholder to mirror.
        let Some(ast_value) = let_stmt.value.as_ref() else {
            use crate::tir::{TirExprKind, TirStmtKind, TypeTable};
            // 7-A: same as the initialised case — read the binding's recorded
            // type (this path always binds a simple `Ident` / `MutIdent`).
            let binding_id = match &let_stmt.pattern {
                ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => Some(*id),
                _ => None,
            };
            let type_id = let_stmt
                .ty
                .as_ref()
                .map(|t| {
                    binding_id
                        .and_then(|id| self.ann_local_type(id))
                        .unwrap_or_else(|| self.resolve_type(t))
                })
                .unwrap_or(TypeTable::UNKNOWN);
            return match &let_stmt.pattern {
                ast::Pattern::Ident { id, name, span: _ }
                | ast::Pattern::MutIdent { id, name, span: _ } => {
                    let is_mut = let_stmt.is_mut
                        || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                    let local_index = ctx.add_local(name.clone(), type_id, is_mut, Some(*id));
                    let placeholder =
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, let_stmt.span);
                    TirStmt::new(
                        TirStmtKind::Let {
                            name: name.clone(),
                            local_index,
                            is_mut,
                            is_reactive: let_stmt.is_reactive,
                            type_id,
                            value: placeholder,
                            skip_value_copy: false,
                        },
                        let_stmt.span,
                    )
                }
                _ => TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Unit,
                        TypeTable::UNIT,
                        let_stmt.span,
                    )),
                    let_stmt.span,
                ),
            };
        };

        // 7-A (E2-thin): a simple binding's annotated type is the
        // scope-sensitive type annotate recorded as the local's type; read it
        // instead of re-resolving the annotation. Destructuring patterns bind
        // per-element, so they keep re-resolving the whole-pattern annotation.
        let simple_binding_id = match &let_stmt.pattern {
            ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => Some(*id),
            _ => None,
        };
        let annotated_type = let_stmt.ty.as_ref().map(|t| {
            simple_binding_id
                .and_then(|id| self.ann_local_type(id))
                .unwrap_or_else(|| self.resolve_type(t))
        });
        let value = self.reify_expr(ast_value, ctx, annotated_type);
        let type_id = annotated_type.unwrap_or(value.type_id);

        match &let_stmt.pattern {
            ast::Pattern::Ident { id, name, span: _ } => {
                // `let mut x = …` carries the mutability on `LetStmt`,
                // not on the `Ident` pattern.
                let is_mut = let_stmt.is_mut;
                let local_index = ctx.add_local(name.clone(), type_id, is_mut, Some(*id));
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::MutIdent { id, name, span: _ } => {
                let local_index = ctx.add_local(name.clone(), type_id, true, Some(*id));
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: true,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Wildcard => {
                // `let _ = expr;` discards. Lower as an Expr stmt.
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
            ast::Pattern::Tuple(_, _)
            | ast::Pattern::Struct { .. }
            | ast::Pattern::Variant { .. } => {
                // Destructuring `let [a, b] = …;` / `let Point { x, y }
                // = …;` / `let Some(x) = …;`. The TIR uses
                // `TirStmtKind::LetDestructure` rather than `Let`. The
                // shared `reify_pattern` adds the sub-pattern bindings
                // to `ctx`; the value's recorded type drives the
                // pattern's per-binding type lookups.
                let pattern = self.reify_pattern(&let_stmt.pattern, type_id, ctx);
                TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Literal(_) | ast::Pattern::Or(_) | ast::Pattern::Range { .. } => {
                let _ = type_id;
                let _ = TypeTable::UNKNOWN;
                // `let 42 = expr;` etc. are refutable patterns and the
                // elaborator rejects them at annotate time (only
                // irrefutable patterns are valid in `let`). Hitting
                // this branch means annotate let a refutable pattern
                // through — surface the invariant violation here.
                panic!(
                    "reify_let: refutable pattern {:?} in let binding (annotate should have rejected)",
                    let_stmt.pattern
                )
            }
        }
    }

    /// Reify an expression. Reads `sem.types.expression_types` for the
    /// type, `sem.types.coercions` for any coercion wrap,
    /// `sem.types.method_dispatch` for method calls,
    /// `sem.types.desugars` for desugar expansions, and
    /// `sem.types.generic_instantiations` for generic call /
    /// struct-literal / variant-ctor type args.
    pub(super) fn reify_expr(
        &mut self,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};

        // Power-assert capture hook (Gap 5). See `reify_assert` /
        // `reify_with_assert_capture`.
        if let Some(actx) = ctx.reify_assert_capture_ctx.as_ref() {
            let ast_id = expr.id();
            if !actx.in_progress.contains(&ast_id)
                && let Some(&slot_idx) = actx.ast_id_to_slot.get(&ast_id)
            {
                return self.reify_with_assert_capture(slot_idx, expr, ctx, expected_type);
            }
        }

        // The expression's recorded type is the source of truth for
        // `TirExpr::type_id`. Falls back to `expected_type` (or
        // `UNKNOWN` when neither is available) for AST shapes that
        // evaporated during annotate (e.g. a stmt-position match
        // whose recorder fires only at the stmt level).
        let recorded_type = self
            .ann_expression_types(expr.id())
            .or(expected_type)
            .unwrap_or(TypeTable::UNKNOWN);
        let span = expr.span();

        // Replay an i128 / u128 numeric-literal coercion: annotate
        // recorded it on `sem.types.coercions`, and unlike every other
        // `NumericLiteral` coercion (which only retags the literal's
        // type) the 128-bit structs need an explicit constructor call.
        if let Some(tir) = self.try_reify_int128_coercion(expr) {
            return tir;
        }

        match expr {
            ast::Expr::Literal(lit) => self.reify_literal(lit, recorded_type, ctx),
            ast::Expr::Block(block) => {
                let block_tir = self.reify_block(block, ctx, expected_type);
                TirExpr::new(TirExprKind::Block(block_tir), recorded_type, span)
            }
            ast::Expr::Ident(ident) => self.reify_ident(ident, recorded_type, ctx),
            ast::Expr::TupleLiteral(tuple_lit) => {
                // SequenceLiteralBuilder coercion: when the elaborator
                // recorded `sequence_coercions[tuple.id]`, the literal
                // was lowered through `Builder::new_literal` /
                // `Builder::push_literal` / `Builder::build`. Reify
                // replays the same desugar deterministically — the
                // `__b` local lands at the same `FunctionContext`
                // index reify reserves for it.
                if let Some(facts) = self.ann_sequence_coercions(tuple_lit.id) {
                    return self.reify_sequence_coercion(tuple_lit, facts, ctx, span);
                }
                self.reify_tuple_literal(tuple_lit, ctx, span)
            }
            ast::Expr::Cast(cast) => {
                // 7-A (E2-thin): resolving `cast.target_type` is a
                // scope-sensitive decision annotate already makes and records
                // as the cast expression's type. Read it instead of
                // re-resolving; re-resolution remains only as a fallback for
                // any node annotate did not type, and is dropped once the
                // contract is proven complete.
                let target_type = self
                    .ann_expression_types(cast.id)
                    .unwrap_or_else(|| self.resolve_type(&cast.target_type));
                // `expr as i128/u128` lowers to a `from_u64` / `from_i64`
                // / `from_pair` constructor call rather than a bare cast,
                // since the 128-bit types are prelude structs. Mirrors
                // `Elaborator::resolve_cast`'s int128 branch.
                if let Some(tir) = self.try_reify_int128_cast(cast, target_type, ctx) {
                    return tir;
                }
                // `expr as Ty` — emit `Cast` with the recorded target
                // type. Numeric vs newtype-cast handling is downstream;
                // reify just produces the shape.
                //
                // Re-type a numeric-literal operand (possibly negated) to
                // the cast's target width. annotate propagates the target
                // type to a *direct* literal cast operand (`9e15 as i64`
                // types the literal `i64`) but not through a `Neg`
                // (`-9e15 as i64` leaves the inner literal `i32`), so at
                // codegen an `i32.const` truncates a value > `i32::MAX`
                // (`2^53 mod 2^32 == 0`) before the cast widens it. Mirror
                // the production resolver, which types the operand at the
                // target width in this position.
                let target_is_int = self.tysys.type_table.borrow().is_integer(target_type);
                let is_number_lit = |e: &ast::Expr| matches!(e, ast::Expr::Literal(l) if matches!(l.value, ast::Literal::Number(_)));
                let inner = if target_is_int && is_number_lit(&cast.expr) {
                    let ast::Expr::Literal(lit) = &cast.expr else {
                        unreachable!()
                    };
                    self.reify_literal(lit, target_type, ctx)
                } else if target_is_int
                    && let ast::Expr::Unary(u) = &cast.expr
                    && u.op == ast::UnaryOp::Neg
                    && is_number_lit(&u.expr)
                {
                    let ast::Expr::Literal(lit) = &u.expr else {
                        unreachable!()
                    };
                    let lit_tir = self.reify_literal(lit, target_type, ctx);
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: crate::tir::TirUnaryOp::Neg,
                            expr: Box::new(lit_tir),
                        },
                        target_type,
                        span,
                    )
                } else {
                    self.reify_expr(&cast.expr, ctx, None)
                };
                TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(inner),
                        target_type,
                    },
                    target_type,
                    span,
                )
            }
            ast::Expr::Unary(unary) => {
                let op = ast_unary_op_to_tir(unary.op);
                // A `-<numeric literal>` operand shares the unary's type:
                // propagate the expected/recorded type so the inner literal
                // takes the right width (e.g. `-1.0` in an `f32` const body
                // must be `f32`, not the default `f64`). Other unary operands
                // are typed on their own.
                let inner_expected = if unary.op == ast::UnaryOp::Neg
                    && self.tysys.is_numeric_literal(&unary.expr)
                    && recorded_type != crate::tir::TypeTable::UNKNOWN
                {
                    Some(recorded_type)
                } else {
                    None
                };
                let inner = self.reify_expr(&unary.expr, ctx, inner_expected);
                if let Some(dispatch) = self.ann_operator_dispatch(unary.id) {
                    // Operator-trait dispatch path for `-x` / `~x` on a
                    // user type (`Neg::neg` / `BitNot::bitnot`). Mirrors the
                    // binary path: a bare `Unary` on a struct operand would
                    // be rejected by codegen (`expected i32, found (ref $T)`),
                    // so replay the recorded method call instead. Unary
                    // operators take no extra arguments.
                    let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                        inner,
                        dispatch.self_kind,
                        /* is_ref_impl */ false,
                        span,
                        &self.tysys.type_table,
                    );
                    return super::Elaborator::<H>::build_tir_method_call(
                        receiver,
                        dispatch.function_ref,
                        vec![],
                        vec![],
                        dispatch.return_type,
                        span,
                    );
                }
                // Constant-fold `-literal` into a negative literal, exactly
                // as `Elaborator::resolve_unary` (operators.rs:949-1004).
                // Without this reify emits `Unary { Neg, <pos literal> }`,
                // which lowers to `i32.sub (const 0) …` / `f64.neg …` and
                // can produce invalid modules (e.g. a negated literal that
                // only fits as the already-negative value, or a type
                // mismatch when the operand's literal type differs).
                if matches!(op, crate::tir::TirUnaryOp::Neg) {
                    match &inner.kind {
                        TirExprKind::IntLiteral { value, repr } => {
                            return TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: (*value as i64).wrapping_neg().cast_unsigned(),
                                    repr: format!("-{repr}"),
                                },
                                inner.type_id,
                                span,
                            );
                        }
                        TirExprKind::FloatLiteral { value, repr } => {
                            return TirExpr::new(
                                TirExprKind::FloatLiteral {
                                    value: -value,
                                    repr: format!("-{repr}"),
                                },
                                inner.type_id,
                                span,
                            );
                        }
                        TirExprKind::Cast {
                            expr: cast_inner,
                            target_type,
                        } if matches!(&cast_inner.kind, TirExprKind::IntLiteral { .. }) => {
                            if let TirExprKind::IntLiteral { value, repr } = &cast_inner.kind {
                                let neg_literal = TirExpr::new(
                                    TirExprKind::IntLiteral {
                                        value: (*value as i64).wrapping_neg().cast_unsigned(),
                                        repr: format!("-{repr}"),
                                    },
                                    cast_inner.type_id,
                                    span,
                                );
                                return TirExpr::new(
                                    TirExprKind::Cast {
                                        expr: Box::new(neg_literal),
                                        target_type: *target_type,
                                    },
                                    *target_type,
                                    span,
                                );
                            }
                        }
                        _ => {}
                    }
                }

                // Track address-taken locals for `&x` / `&mut x`, mirroring
                // `Elaborator::resolve_unary` (operators.rs:834). The
                // boxing pass (`lower::plan::boxing`) reads
                // `TirFunction::address_taken_locals` to retag a borrowed
                // local's declaration to its box type, so that mutation
                // through the reference (e.g. `*slot = other_fn`) writes
                // back to the original slot. Without this the local stays
                // unboxed and `&mut local` boxes a throwaway copy.
                if matches!(
                    op,
                    crate::tir::TirUnaryOp::Ref | crate::tir::TirUnaryOp::MutRef
                ) && let TirExprKind::Local { index, .. } = &inner.kind
                {
                    ctx.address_taken_locals.insert(*index);
                }
                TirExpr::new(
                    TirExprKind::Unary {
                        op,
                        expr: Box::new(inner),
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::MethodCall(method_call) => {
                self.reify_method_call(method_call, ctx, recorded_type)
            }
            ast::Expr::Binary(binary) => self.reify_binary(binary, ctx, recorded_type),
            ast::Expr::Call(call) => self.reify_call(call, ctx, recorded_type),
            ast::Expr::Match(match_expr) => {
                self.reify_match_expr(match_expr, ctx, expected_type, recorded_type)
            }
            ast::Expr::StructLiteral(struct_lit) => {
                self.reify_struct_literal(struct_lit, ctx, recorded_type)
            }
            ast::Expr::Range(range) => self.reify_range(range, ctx, recorded_type),
            ast::Expr::TemplateString(template) => {
                self.reify_template_string(template, ctx, recorded_type)
            }
            ast::Expr::Matches(m) => self.reify_matches(m, ctx),
            ast::Expr::CompoundAssign(compound) => {
                self.reify_compound_assign(compound, ctx, recorded_type)
            }
            ast::Expr::TryOp(qm) => self.reify_question_mark(qm, ctx, recorded_type),
            ast::Expr::Closure(closure) => {
                self.reify_closure(closure, ctx, recorded_type, expected_type)
            }
            ast::Expr::Index(index) => self.reify_index(index, ctx, recorded_type),
            ast::Expr::ComparisonChain(chain) => self.reify_comparison_chain(chain, ctx),
            ast::Expr::StaticMethodCall(static_call) => {
                self.reify_static_method_call(static_call, ctx, recorded_type)
            }
            ast::Expr::Resume(resume) => {
                // `resume value` inside a handler method. Reify the
                // value with the function's return type as expected
                // (matches `Elaborator::resolve_resume` at
                // handlers.rs:445), then emit `TirExprKind::Resume`.
                let expected = if ctx.in_handler_method {
                    Some(ctx.return_type)
                } else {
                    None
                };
                let value = self.reify_expr(&resume.value, ctx, expected);
                TirExpr::new(
                    TirExprKind::Resume {
                        value: Box::new(value),
                    },
                    crate::tir::TypeTable::UNIT,
                    span,
                )
            }
            ast::Expr::LabeledBlock(lb) => {
                // Match `Elaborator::resolve_expr`'s `LabeledBlock`
                // arm (expr.rs:234–305): push a `LabeledBlockTarget`
                // so any `break label: expr` inside lowers via this
                // frame, walk the inner block, pop the frame, emit
                // `TirExprKind::LabeledBlock`. The result type is the
                // recorded `expression_types[lb.id]`; annotate already
                // unified break types into it.
                use crate::elaborator::types::LabeledBlockTarget;
                // Fall back to the block's unified result type when the use
                // site supplies no expected type, so a `break label: null`
                // whose `Option<T>` only resolves from a sibling break still
                // coerces (annotate unified the breaks into `recorded_type`).
                ctx.labeled_block_targets.push(LabeledBlockTarget {
                    label: lb.label.clone(),
                    break_types: Vec::new(),
                    expected_type: expected_type.or(Some(recorded_type)),
                });
                ctx.active_labels.push(lb.label.clone());
                let tir_block = self.reify_block(&lb.block, ctx, expected_type);
                ctx.active_labels.pop();
                let _target = ctx.labeled_block_targets.pop();
                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: lb.label.clone(),
                        block: tir_block,
                        result_type: recorded_type,
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::Spread(_, _) => {
                // `Spread` is only valid inside a tuple literal; the
                // elaborator panics if it sees one at top level.
                // Mirror the panic — annotate would have already
                // diagnosed a stray spread.
                panic!("reify_expr: bare Spread is invalid outside TupleLiteral")
            }
            ast::Expr::If(if_expr) => {
                self.reify_if_expr(if_expr, ctx, expected_type, recorded_type)
            }
            ast::Expr::Assign(assign) => {
                // IndexAssign rewrite: `arr[i] = v` lowers to
                // `arr.index_assign(i, v)`. The elaborator's
                // `assign_to_target` records the resolved
                // `FunctionRef` on `index_assign_dispatch[index.id]`;
                // reify replays the same `MethodCall` shape.
                if let ast::Expr::Index(index_expr) = &assign.target
                    && let Some(dispatch) = self.ann_index_assign_dispatch(index_expr.id)
                {
                    let receiver = self.reify_expr(&index_expr.expr, ctx, None);
                    let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                        receiver,
                        dispatch.self_kind,
                        false,
                        span,
                        &self.tysys.type_table,
                    );
                    let idx_expr = self.reify_expr(&index_expr.index, ctx, None);
                    let value_expr = self.reify_expr(&assign.value, ctx, None);
                    return super::Elaborator::<H>::build_tir_method_call(
                        receiver,
                        dispatch.function_ref,
                        vec![],
                        vec![
                            crate::tir::CallArg::new(idx_expr, false),
                            crate::tir::CallArg::new(value_expr, false),
                        ],
                        dispatch.return_type,
                        span,
                    );
                }
                // `target = value` — both sides walked recursively; the
                // expression's type is `Unit` (assignment is a stmt-shape
                // expression in Wado, mirroring Rust).
                let target = self.reify_expr(&assign.target, ctx, None);
                let value = self.reify_expr(&assign.value, ctx, Some(target.type_id));
                // Global-var write: `g = v` lowers to `GlobalVarSet` so
                // codegen actually mutates the global. The production
                // `assign_to_target` rewrites here too (operators.rs:1192+).
                if let TirExprKind::GlobalVarGet {
                    module_source,
                    name,
                } = &target.kind
                {
                    return TirExpr::new(
                        TirExprKind::GlobalVarSet {
                            module_source: module_source.clone(),
                            name: name.clone(),
                            value: Box::new(value),
                        },
                        crate::tir::TypeTable::UNIT,
                        span,
                    );
                }
                TirExpr::new(
                    TirExprKind::Assign {
                        target: Box::new(target),
                        value: Box::new(value),
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::FieldAccess(field_access) => {
                // The `field_index` and `field_name` on `FieldAccess`
                // TIR are positional; the elaborator looks them up from
                // the receiver's struct decl. Reify reads the same
                // info from `tysys.all_struct_fields` keyed by the
                // receiver's resolved struct name.
                let inner = self.reify_expr(&field_access.expr, ctx, None);
                let (field_index, field_name, field_type) =
                    self.lookup_struct_field_index(inner.type_id, &field_access.field);
                TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(inner),
                        field_index,
                        field_name,
                    },
                    field_type.unwrap_or(recorded_type),
                    span,
                )
            }
            ast::Expr::WithHandler(with_expr) => self.reify_with_handler(with_expr, ctx),
        }
    }

    /// Reify a `while cond { body }` statement. Mirrors
    /// `Elaborator::resolve_while`'s `Condition::Expr` arm
    /// (stmt.rs:2982+): the loop lowers into
    /// `Loop { if !cond { break; } body }`, which is the desugar
    /// `DesugarKind::While` tags. `for_continue_labels` is saved /
    /// restored around the body walk so naked `continue` inside
    /// `while` targets this loop (not an enclosing C-style `for`).
    fn reify_while(&mut self, w: &ast::WhileStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable};

        let span = w.span;
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);

        let stmts = match &w.condition {
            ast::Condition::Expr(cond_expr) => {
                let cond_span = cond_expr.span();
                let cond_tir = self.reify_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let neg_cond = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(cond_tir),
                    },
                    TypeTable::BOOL,
                    cond_span,
                );
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let if_break = TirStmt::new(
                    TirStmtKind::If {
                        condition: neg_cond,
                        then_block: TirBlock::new(vec![break_stmt], span),
                        else_block: None,
                    },
                    span,
                );
                let body_block = self.reify_block(&w.body, ctx, None);
                let mut stmts = Vec::with_capacity(1 + body_block.stmts.len());
                stmts.push(if_break);
                stmts.extend(body_block.stmts);
                stmts
            }
            ast::Condition::LetChain {
                elements,
                span: cond_span,
            } => {
                // Mirror `Elaborator::resolve_while`'s LetChain
                // arm (stmt.rs:3016+): the else-branch
                // unconditionally `break`s out of the loop.
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let else_block = TirBlock::new(vec![break_stmt], *cond_span);
                ctx.enter_scope();
                let body_stmts = self.reify_let_chain_stmts(
                    elements,
                    &w.body,
                    Some(&else_block),
                    ctx,
                    None,
                    *cond_span,
                );
                ctx.exit_scope();
                body_stmts
            }
        };

        ctx.for_continue_labels = saved_continue;
        vec![TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(stmts, span),
            },
            span,
        )]
    }

    /// Reify hook for a power-assert-flagged sub-expression: reifies
    /// the sub-tree under an `in_progress` guard, allocates `__vK`,
    /// pushes the `let __vK = …;` onto the capture context, and
    /// returns `Local(__vK)` for the surrounding reify to splice in.
    /// Mirrors [`super::Elaborator::resolve_with_assert_capture`]
    /// (assert.rs:373+).
    fn reify_with_assert_capture(
        &mut self,
        slot_idx: usize,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<crate::tir::TypeId>,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStmtKind};

        let ast_id = expr.id();
        ctx.reify_assert_capture_ctx
            .as_mut()
            .expect("reify_assert_capture_ctx present (guarded by caller)")
            .in_progress
            .insert(ast_id);

        let resolved = self.reify_expr(expr, ctx, expected_type);

        ctx.reify_assert_capture_ctx
            .as_mut()
            .expect("reify_assert_capture_ctx survives recursive reify")
            .in_progress
            .shift_remove(&ast_id);

        let type_id = resolved.type_id;
        let cap_span = resolved.span;
        let cap_name = ctx
            .reify_assert_capture_ctx
            .as_ref()
            .expect("reify_assert_capture_ctx survives recursive reify")
            .slots[slot_idx]
            .name
            .clone();

        // `defining_ast_id = None` keeps synthetic locals out of
        // `local_symbols` (LSP hover / go-to-def).
        let local_index = ctx.add_local(cap_name.clone(), type_id, false, None);

        let cap_ctx = ctx
            .reify_assert_capture_ctx
            .as_mut()
            .expect("reify_assert_capture_ctx survives recursive reify");
        cap_ctx.slots[slot_idx].emitted = true;
        cap_ctx.slots[slot_idx].local_index = Some(local_index);
        cap_ctx.slots[slot_idx].type_id = Some(type_id);
        cap_ctx.emitted_lets.push(TirStmt::new(
            TirStmtKind::Let {
                name: cap_name.clone(),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: resolved,
                skip_value_copy: false,
            },
            cap_span,
        ));

        TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name: cap_name,
            },
            type_id,
            cap_span,
        )
    }

    /// Reify `assert cond[, msg];` into the power-assert expansion.
    /// Mirrors [`super::Elaborator::desugar_assert`] (assert.rs:65+).
    /// Capture slots come from the recorded
    /// [`super::sem::types::AssertCaptureInfo`] (annotate's
    /// `CaptureScanner` already chose them); the hook in `reify_expr`
    /// emits the `let __vK = …;` bindings during the condition walk.
    fn reify_assert(
        &mut self,
        assert_stmt: &ast::AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::{
            CallArg, FunctionRef, TirBlock, TirExprKind, TirStmtKind, TirTemplatePart, TirUnaryOp,
            TypeTable,
        };

        let span = assert_stmt.span;

        // Always install the context: an empty `ast_id_to_slot` map
        // intercepts nothing, and the hook is a single Option check.
        let info = self.ann_assert_captures(assert_stmt.id);
        let (slot_meta, ast_id_to_slot): (Vec<(AstId, String)>, IndexMap<AstId, usize>) =
            if let Some(info) = info.as_ref() {
                let mut meta: Vec<(AstId, String)> = Vec::with_capacity(info.slots.len());
                let mut map: IndexMap<AstId, usize> = IndexMap::default();
                for (i, s) in info.slots.iter().enumerate() {
                    meta.push((s.ast_id, s.capture_label.clone()));
                    map.insert(s.ast_id, i);
                }
                (meta, map)
            } else {
                (Vec::new(), IndexMap::default())
            };

        ctx.enter_scope();

        ctx.reify_assert_capture_ctx = Some(ReifyAssertCaptureContext {
            slots: slot_meta
                .iter()
                .enumerate()
                .map(|(i, (ast_id, label))| ReifyAssertSlot {
                    ast_id: *ast_id,
                    name: format!("__v{i}"),
                    label: label.clone(),
                    emitted: false,
                    local_index: None,
                    type_id: None,
                })
                .collect(),
            ast_id_to_slot,
            in_progress: IndexSet::default(),
            emitted_lets: Vec::new(),
        });

        // `expected_type = None`: a `Bool` expectation propagates into
        // `If`/`Match` branches inside the condition and rejects
        // non-bool arm bodies. Mirrors assert.rs:108.
        let cond_tir = self.reify_expr(&assert_stmt.condition, ctx, None);

        let actx = ctx
            .reify_assert_capture_ctx
            .take()
            .expect("reify_assert_capture_ctx survives condition reify");

        let mut inner_stmts: Vec<TirStmt> = Vec::with_capacity(actx.emitted_lets.len() + 2);
        inner_stmts.extend(actx.emitted_lets);

        let cond_type = cond_tir.type_id;
        let cond_name = "__cond".to_string();
        let cond_local_index = ctx.add_local(cond_name.clone(), cond_type, false, None);
        inner_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: cond_name.clone(),
                local_index: cond_local_index,
                is_mut: false,
                is_reactive: false,
                type_id: cond_type,
                value: cond_tir,
                skip_value_copy: false,
            },
            span,
        ));

        let cond_ref = TirExpr::new(
            TirExprKind::Local {
                index: cond_local_index,
                name: cond_name,
            },
            cond_type,
            span,
        );
        let neg_cond = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(cond_ref),
            },
            TypeTable::BOOL,
            span,
        );

        // Panic template, mirroring `build_assert_panic_template`
        // (assert.rs:239+): header + `condition: <source>` + one
        // `<label>: {__vK:?}` line per emitted slot.
        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        let line = span.line as u64;
        let mut parts: Vec<TirTemplatePart> = vec![
            TirTemplatePart::Literal("Assertion failed in ".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::StringLiteral(ctx.function_name.clone()),
                    string_type,
                    span,
                )),
                format_spec: None,
            },
            TirTemplatePart::Literal(" at ".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::StringLiteral(self.current_module_source.to_string()),
                    string_type,
                    span,
                )),
                format_spec: None,
            },
            TirTemplatePart::Literal(":".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    TypeTable::I32,
                    span,
                )),
                format_spec: None,
            },
        ];
        if let Some(msg) = &assert_stmt.message {
            parts.push(TirTemplatePart::Literal(": ".to_string()));
            let msg_tir = self.reify_expr(msg, ctx, None);
            parts.push(TirTemplatePart::Interpolation {
                expr: Box::new(msg_tir),
                format_spec: None,
            });
        }

        let condition_source = crate::unparse::unparse_expr_simple(&assert_stmt.condition);
        parts.push(TirTemplatePart::Literal(format!(
            "\ncondition: {condition_source}\n"
        )));

        for slot in &actx.slots {
            if !slot.emitted {
                continue;
            }
            let (Some(local_index), Some(type_id)) = (slot.local_index, slot.type_id) else {
                continue;
            };
            parts.push(TirTemplatePart::Literal(format!("{}: ", slot.label)));
            let local_ref = TirExpr::new(
                TirExprKind::Local {
                    index: local_index,
                    name: slot.name.clone(),
                },
                type_id,
                span,
            );
            parts.push(TirTemplatePart::Interpolation {
                expr: Box::new(local_ref),
                format_spec: Some(crate::tir::TemplateFormatSpec {
                    fill: None,
                    align: None,
                    sign_plus: false,
                    alternate: false,
                    zero_pad: false,
                    width: None,
                    precision: None,
                    type_char: Some('?'),
                }),
            });
            parts.push(TirTemplatePart::Literal("\n".to_string()));
        }

        let template_tir = TirExpr::new(TirExprKind::TemplateString { parts }, string_type, span);

        let panic_module_source = self.interner.borrow_mut().core("internal");
        let panic_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: panic_module_source,
                    name: "panic".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: Vec::new(),
                args: vec![CallArg::new(template_tir, false)],
            },
            TypeTable::NEVER,
            span,
        );

        let then_block = TirBlock::new(
            vec![TirStmt::new(TirStmtKind::Expr(panic_call), span)],
            span,
        );
        inner_stmts.push(TirStmt::new(
            TirStmtKind::If {
                condition: neg_cond,
                then_block,
                else_block: None,
            },
            span,
        ));

        ctx.exit_scope();

        // Wrap in `__assert_N:` LabeledBlock so the synthetic
        // counter on `FunctionContext` advances in lockstep with
        // annotate's allocation (Gap 7 walk-order invariant).
        let assert_serial = ctx.next_assert_id;
        ctx.next_assert_id += 1;
        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: format!("__assert_{assert_serial}"),
                block: TirBlock::new(inner_stmts, span),
            },
            span,
        )]
    }

    /// Reify a `for x of expr { body }` loop. Reads the
    /// `DesugarKind` tag (`ForOfTuple` / `ForOfVariadic` /
    /// `ForOfIterator`) annotate placed on `for_of.id` to pick the
    /// expansion path.
    ///
    /// - `ForOfIterator` consumes Gap 6's
    ///   `for_of_iterator` record and emits
    ///   `match next() { Some(v) => body, _ => break }`.
    /// - `ForOfTuple` compile-time-unrolls into per-element
    ///   labelled blocks (mirrors `resolve_tuple_for_of`),
    ///   handling the `.enumerate()` unwrap.
    /// - `ForOfVariadic` emits a deferred `VariadicForOf` TIR
    ///   node the monomorphizer expands after `TypePack`
    ///   substitution.
    fn reify_for_of(&mut self, for_of: &ast::ForOfStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirExprKind, TirStmtKind, TypeTable};

        match self.ann_desugars(for_of.id) {
            Some(super::sem::types::DesugarKind::ForOfTuple) => {
                self.reify_tuple_for_of(for_of, ctx)
            }
            Some(super::sem::types::DesugarKind::ForOfVariadic) => {
                self.reify_variadic_for_of(for_of, ctx)
            }
            Some(super::sem::types::DesugarKind::ForOfIterator) | None => {
                let Some(info) = self.ann_for_of_iterator(for_of.id) else {
                    return vec![TirStmt::new(
                        TirStmtKind::Expr(TirExpr::new(
                            TirExprKind::Unit,
                            TypeTable::UNIT,
                            for_of.span,
                        )),
                        for_of.span,
                    )];
                };
                self.reify_iterator_for_of(for_of, ctx, info)
            }
            _ => unreachable!("for_of carries one of the three ForOf* desugar tags"),
        }
    }

    /// `IntoIterator` path of for-of (extracted from
    /// `reify_for_of` for readability). The Gap 6 record carries
    /// the resolved `into_iter` / `next` `FunctionRef`s.
    fn reify_iterator_for_of(
        &mut self,
        for_of: &ast::ForOfStmt,
        ctx: &mut FunctionContext,
        info: super::sem::types::ForOfIteratorInfo,
    ) -> Vec<TirStmt> {
        use crate::tir::{
            CallArg, ResolvedType, TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind,
            TypeTable,
        };

        let span = for_of.span;
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);
        let unique_id = ctx.next_local;
        let iter_var = format!("__iter_{unique_id}");
        let label = format!("__for_of_{unique_id}");

        let into_iter_receiver = self.reify_expr(&for_of.iterable, ctx, None);
        let into_iter_receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
            into_iter_receiver,
            info.into_iter_self_kind,
            info.into_iter_is_ref_impl,
            span,
            &self.tysys.type_table,
        );
        let iter_type = info.iter_type;
        let into_iter_call = super::Elaborator::<H>::build_tir_method_call(
            into_iter_receiver,
            info.into_iter.clone(),
            vec![],
            vec![],
            iter_type,
            span,
        );

        let iter_local_index =
            ctx.add_local(iter_var.clone(), iter_type, /* is_mut */ true, None);
        let iter_let = TirStmt::new(
            TirStmtKind::Let {
                name: iter_var.clone(),
                local_index: iter_local_index,
                is_mut: true,
                is_reactive: false,
                type_id: iter_type,
                value: into_iter_call,
                skip_value_copy: false,
            },
            span,
        );

        ctx.active_labels.push(label.clone());

        let iter_local_ref = TirExpr::new(
            TirExprKind::Local {
                index: iter_local_index,
                name: iter_var,
            },
            iter_type,
            span,
        );
        let next_receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
            iter_local_ref,
            info.next_self_kind,
            info.next_is_ref_impl,
            span,
            &self.tysys.type_table,
        );
        let option_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_option(info.item_type);
        let next_call = super::Elaborator::<H>::build_tir_method_call(
            next_receiver,
            info.next.clone(),
            vec![],
            vec![],
            option_type,
            span,
        );

        let some_case_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
            .to_string();

        ctx.enter_scope();
        let binding_pattern = self.reify_pattern(&for_of.binding, info.item_type, ctx);
        let body_block = self.reify_block(&for_of.body, ctx, None);
        ctx.exit_scope();

        let some_pattern = TirPattern::Variant {
            enum_type: option_type,
            variant_name: some_case_name,
            bindings: vec![binding_pattern],
            payload_type: info.item_type,
        };

        let body_type = match &body_block.stmts.last() {
            Some(stmt) => match &stmt.kind {
                TirStmtKind::Expr(e) => e.type_id,
                _ => TypeTable::UNIT,
            },
            None => TypeTable::UNIT,
        };
        let some_body = TirExpr::new(TirExprKind::Block(body_block), body_type, span);

        let break_block = TirBlock::new(
            vec![TirStmt::new(
                TirStmtKind::Break {
                    label: None,
                    value: None,
                },
                span,
            )],
            span,
        );
        let break_body = TirExpr::new(TirExprKind::Block(break_block), TypeTable::UNIT, span);

        let match_type =
            crate::tir::agree_branch_types(body_type, TypeTable::UNIT).unwrap_or(TypeTable::UNIT);
        let arms = vec![
            TirMatchArm {
                pattern: some_pattern,
                guard: None,
                body: some_body,
                span,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: break_body,
                span,
            },
        ];
        let match_expr = TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(next_call),
                arms,
            },
            match_type,
            span,
        );
        let loop_body = TirBlock::new(
            vec![TirStmt::new(TirStmtKind::Expr(match_expr), span)],
            span,
        );
        let loop_tir = TirStmt::new(TirStmtKind::Loop { body: loop_body }, span);

        ctx.active_labels.pop();
        ctx.for_continue_labels = saved_continue;

        let _ = CallArg::new(
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
            false,
        );
        let _ = ResolvedType::Unit;

        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label,
                block: TirBlock::new(vec![iter_let, loop_tir], span),
            },
            span,
        )]
    }

    /// Compile-time-unroll a tuple for-of into per-element
    /// labelled blocks. Mirrors `Elaborator::resolve_tuple_for_of`
    /// (stmt.rs:2361+). Handles `.enumerate()` unwrap on the AST
    /// receiver so each iteration sees `[i, element]`.
    fn reify_tuple_for_of(
        &mut self,
        for_of: &ast::ForOfStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TypeTable};

        let span = for_of.span;
        let unique_id = ctx.next_local;

        let (actual_iterable, is_enumerate) = match &for_of.iterable {
            ast::Expr::MethodCall(mc) if mc.method == "enumerate" && mc.args.is_empty() => {
                (&mc.receiver, true)
            }
            other => (other, false),
        };
        let iterable = self.reify_expr(actual_iterable, ctx, None);
        let tuple_type_id = iterable.type_id;
        let elems: Vec<TypeId> = self
            .tysys
            .type_table
            .borrow()
            .as_tuple(tuple_type_id)
            .unwrap_or_default();

        let temp_name = format!("__tuple_{unique_id}");
        let temp_local = ctx.add_local(temp_name.clone(), tuple_type_id, false, None);
        let temp_let = TirStmt::new(
            TirStmtKind::Let {
                name: temp_name.clone(),
                local_index: temp_local,
                is_mut: false,
                is_reactive: false,
                type_id: tuple_type_id,
                value: iterable,
                skip_value_copy: false,
            },
            span,
        );

        let mut outer_stmts = vec![temp_let];

        // Consume this for-of's overlays for the current instantiation.
        // Annotate pushed one per-element overlay set per instantiation in
        // walk order; the visit counter selects the matching one (a nested
        // inner for-of is instantiated once per outer element). Each
        // element's overlay is pushed onto `tuple_overlay_stack` while its
        // binding and body are reified so the `ann_*` accessors see the
        // right per-element facts instead of the truncated base maps.
        let instantiation: Vec<super::sem::types::ElementOverlay> = {
            let for_of_key =
                crate::symbol::SymbolKey::new(self.current_module_source.clone(), for_of.id);
            let visit = self
                .tuple_overlay_visits
                .entry(for_of_key.clone())
                .or_insert(0);
            let k = *visit;
            *visit += 1;
            self.sem
                .types
                .tuple_overlays
                .get(&for_of_key)
                .and_then(|insts| insts.get(k))
                .cloned()
                .unwrap_or_default()
        };

        for (i, &elem_type) in elems.iter().enumerate() {
            ctx.enter_scope();
            if let Some(overlay) = instantiation.get(i) {
                self.tuple_overlay_stack.push(overlay.clone());
            }

            let temp_ref = TirExpr::new(
                TirExprKind::Local {
                    index: temp_local,
                    name: temp_name.clone(),
                },
                tuple_type_id,
                span,
            );
            let field_access = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(temp_ref),
                    field_index: i as u32,
                    field_name: i.to_string(),
                },
                elem_type,
                span,
            );

            let mut block_stmts = Vec::new();

            if is_enumerate {
                let i32_type = TypeTable::I32;
                let index_literal = TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: i as u64,
                        repr: i.to_string(),
                    },
                    i32_type,
                    span,
                );
                let enum_tuple_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_tuple(vec![i32_type, elem_type]);
                let enum_tuple = TirExpr::new(
                    TirExprKind::TupleLiteral {
                        elements: vec![index_literal, field_access],
                    },
                    enum_tuple_type,
                    span,
                );
                let tir_pattern = self.reify_pattern(&for_of.binding, enum_tuple_type, ctx);
                block_stmts.push(TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern: tir_pattern,
                        is_mut: for_of.is_mut,
                        value: enum_tuple,
                    },
                    span,
                ));
            } else {
                match &for_of.binding {
                    ast::Pattern::Ident { id, name, span: _ }
                    | ast::Pattern::MutIdent { id, name, span: _ } => {
                        let is_mut = for_of.is_mut
                            || matches!(&for_of.binding, ast::Pattern::MutIdent { .. });
                        let local_index = ctx.add_local(name.clone(), elem_type, is_mut, Some(*id));
                        block_stmts.push(TirStmt::new(
                            TirStmtKind::Let {
                                name: name.clone(),
                                local_index,
                                is_mut,
                                is_reactive: false,
                                type_id: elem_type,
                                value: field_access,
                                skip_value_copy: false,
                            },
                            span,
                        ));
                    }
                    ast::Pattern::Tuple(_, _) | ast::Pattern::Struct { .. } => {
                        let tir_pattern = self.reify_pattern(&for_of.binding, elem_type, ctx);
                        block_stmts.push(TirStmt::new(
                            TirStmtKind::LetDestructure {
                                pattern: tir_pattern,
                                is_mut: for_of.is_mut,
                                value: field_access,
                            },
                            span,
                        ));
                    }
                    ast::Pattern::Wildcard => {
                        block_stmts.push(TirStmt::new(TirStmtKind::Expr(field_access), span));
                    }
                    _ => {
                        // Annotate diagnosed; emit nothing.
                    }
                }
            }

            let body = self.reify_block(&for_of.body, ctx, None);
            block_stmts.extend(body.stmts);

            if instantiation.get(i).is_some() {
                self.tuple_overlay_stack.pop();
            }
            ctx.exit_scope();

            outer_stmts.push(TirStmt::new(
                TirStmtKind::LabeledBlock {
                    label: format!("__tuple_iter_{unique_id}_{i}"),
                    block: TirBlock::new(block_stmts, span),
                },
                span,
            ));
        }

        let label = format!("__tuple_for_of_{unique_id}");
        ctx.active_labels.push(label.clone());
        let result = vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label,
                block: TirBlock::new(outer_stmts, span),
            },
            span,
        )];
        ctx.active_labels.pop();
        result
    }

    /// Emit a deferred `VariadicForOf` TIR node for tuples whose
    /// element types contain `TypePack`. The monomorphizer expands
    /// this after `TypePack` substitution. Mirrors
    /// `Elaborator::resolve_variadic_for_of` (stmt.rs:2223+).
    fn reify_variadic_for_of(
        &mut self,
        for_of: &ast::ForOfStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::{ResolvedType, TirExprKind, TirStmtKind, TypeTable};

        let span = for_of.span;
        let iterable = self.reify_expr(&for_of.iterable, ctx, None);
        let unique_id = ctx.next_local;

        let binding_type = {
            let type_table = self.tysys.type_table.borrow();
            if let Some(elems) = type_table.as_tuple(iterable.type_id) {
                if let Some(tp) = elems
                    .iter()
                    .find(|e| matches!(type_table.get(**e), ResolvedType::TypePack { .. }))
                {
                    *tp
                } else if let Some(first) = elems.first() {
                    *first
                } else {
                    TypeTable::UNKNOWN
                }
            } else {
                TypeTable::UNKNOWN
            }
        };

        let (binding_name, binding_id) = match &for_of.binding {
            ast::Pattern::Ident { id, name, .. } => (name.clone(), Some(*id)),
            ast::Pattern::Tuple(..) => (format!("__pattern_temp_{unique_id}"), None),
            _ => {
                return vec![TirStmt::new(TirStmtKind::Expr(iterable), span)];
            }
        };

        let is_mut = for_of.is_mut;
        ctx.enter_scope();
        let binding_local = ctx.add_local(binding_name.clone(), binding_type, is_mut, binding_id);

        // Destructured binding (`for let [a, b] of …`): bind each inner
        // pattern variable to its element type and prepend a field-access
        // `Let` reading it from the synthetic pair temp, mirroring
        // `resolve_variadic_for_of` (stmt.rs:2259+). Without this the inner
        // names (`a`, `b`) never enter scope, so the body resolves them to
        // `Unknown` — e.g. `a != b` in the variadic `Eq for [..T]` impl
        // dispatches to a nonexistent `unknown^Eq::eq`.
        let mut destruct_stmts: Vec<TirStmt> = Vec::new();
        if let ast::Pattern::Tuple(tp, _) = &for_of.binding {
            let inner_elems = self
                .tysys
                .type_table
                .borrow()
                .as_tuple(binding_type)
                .unwrap_or_else(|| vec![binding_type]);
            for (i, pat_elem) in tp.iter().enumerate() {
                if let ast::Pattern::Ident { id, name, .. } = pat_elem {
                    let elem_type = inner_elems.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                    let local_idx = ctx.add_local(name.clone(), elem_type, is_mut, Some(*id));
                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: binding_local,
                                    name: binding_name.clone(),
                                },
                                binding_type,
                                span,
                            )),
                            field_index: i as u32,
                            field_name: i.to_string(),
                        },
                        elem_type,
                        span,
                    );
                    destruct_stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name: name.clone(),
                            local_index: local_idx,
                            is_mut,
                            is_reactive: false,
                            type_id: elem_type,
                            value: field_access,
                            skip_value_copy: false,
                        },
                        span,
                    ));
                }
            }
        }

        let mut body = self.reify_block(&for_of.body, ctx, None);
        ctx.exit_scope();
        if !destruct_stmts.is_empty() {
            destruct_stmts.extend(body.stmts);
            body.stmts = destruct_stmts;
        }

        vec![TirStmt::new(
            TirStmtKind::VariadicForOf {
                iterable,
                binding_name,
                binding_local,
                is_mut,
                body,
                unique_id,
            },
            span,
        )]
    }

    /// Reify a C-style `for init; cond; update { body }` loop into
    /// the shape `Elaborator::resolve_for` produces (stmt.rs:3095+).
    /// Implements the `Condition::Expr` arm; `Condition::LetChain`
    /// shares the let-chain expansion with `if let` / `while let`
    /// and routes through the same pending `todo!`.
    fn reify_for(&mut self, f: &ast::ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable};

        let span = f.span;
        let loop_id = ctx.next_loop_id;
        ctx.next_loop_id += 1;
        let body_label = format!("__for_{loop_id}_body");

        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);
        ctx.enter_scope();

        let mut outer_stmts: Vec<TirStmt> = Vec::new();
        if let Some(init) = &f.init {
            outer_stmts.extend(self.reify_stmt(init, ctx));
        }

        let iter_stmts: Vec<TirStmt> = match &f.condition {
            None => {
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![labeled_body];
                s.extend(self.reify_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(ast::Condition::Expr(cond_expr)) => {
                let cond_span = cond_expr.span();
                let cond_tir = self.reify_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let neg_cond = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(cond_tir),
                    },
                    TypeTable::BOOL,
                    cond_span,
                );
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let if_break = TirStmt::new(
                    TirStmtKind::If {
                        condition: neg_cond,
                        then_block: TirBlock::new(vec![break_stmt], span),
                        else_block: None,
                    },
                    span,
                );
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![if_break, labeled_body];
                s.extend(self.reify_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(ast::Condition::LetChain {
                elements,
                span: cond_span,
            }) => {
                // For-let-chain is restricted to a single Let
                // element (the parser enforces this; mirror
                // `Elaborator::resolve_for` stmt.rs:3164+). The
                // expansion shape is a single Match: the pattern
                // arm's body is the labeled-body + update; the
                // wildcard arm breaks.
                use crate::tir::{TirExprKind, TirMatchArm, TirPattern};
                let single_let = if elements.len() == 1 {
                    match &elements[0] {
                        ast::ConditionElement::Let {
                            pattern,
                            expr,
                            span: elem_span,
                        } => Some((pattern, expr, *elem_span)),
                        _ => None,
                    }
                } else {
                    None
                };
                let Some((pattern, expr, elem_span)) = single_let else {
                    // Annotate already diagnosed multi-element
                    // for-let-chain as `InvalidPattern`; emit
                    // empty to mirror.
                    ctx.exit_scope();
                    ctx.for_continue_labels = saved_continue;
                    return vec![];
                };

                let scrutinee = self.reify_expr(expr, ctx, None);
                let scrutinee_type = scrutinee.type_id;
                ctx.enter_scope();
                let tir_pattern = self.reify_pattern(pattern, scrutinee_type, ctx);
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let update_stmts = self.reify_for_update(f.update.as_ref(), ctx);
                ctx.exit_scope();

                let mut then_stmts = vec![labeled_body];
                then_stmts.extend(update_stmts);
                let then_body = TirExpr::new(
                    TirExprKind::Block(TirBlock::new(then_stmts, *cond_span)),
                    TypeTable::UNIT,
                    *cond_span,
                );
                let else_body = TirExpr::new(
                    TirExprKind::Block(TirBlock::new(
                        vec![TirStmt::new(
                            TirStmtKind::Break {
                                label: None,
                                value: None,
                            },
                            span,
                        )],
                        *cond_span,
                    )),
                    TypeTable::NEVER,
                    *cond_span,
                );
                let arms = vec![
                    TirMatchArm {
                        pattern: tir_pattern,
                        guard: None,
                        body: then_body,
                        span: elem_span,
                    },
                    TirMatchArm {
                        pattern: TirPattern::Wildcard,
                        guard: None,
                        body: else_body,
                        span: *cond_span,
                    },
                ];
                vec![TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Match {
                            expr: Box::new(scrutinee),
                            arms,
                        },
                        TypeTable::UNIT,
                        *cond_span,
                    )),
                    *cond_span,
                )]
            }
        };

        outer_stmts.push(TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(iter_stmts, span),
            },
            span,
        ));

        ctx.exit_scope();
        ctx.for_continue_labels = saved_continue;
        outer_stmts
    }

    /// Reify the for-loop body wrapped in `__for_N_body:` so naked
    /// `continue` lowers as `break __for_N_body` (letting the
    /// `update` expression run before the next iteration). Mirrors
    /// `Elaborator::resolve_for_labeled_body` (stmt.rs:3280+).
    fn reify_for_labeled_body(
        &mut self,
        body_label: &str,
        body: &ast::Block,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        use crate::tir::TirStmtKind;
        ctx.for_continue_labels.push(body_label.to_string());
        ctx.active_labels.push(body_label.to_string());
        let body_block = self.reify_block(body, ctx, None);
        ctx.active_labels.pop();
        ctx.for_continue_labels.pop();
        TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: body_label.to_string(),
                block: body_block,
            },
            body.span,
        )
    }

    /// Reify the for-loop's optional `update` expression as a single
    /// stmt-list (empty when absent). Mirrors
    /// `Elaborator::resolve_for_update` (stmt.rs:3302+).
    fn reify_for_update(
        &mut self,
        update: Option<&ast::Expr>,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        update
            .map(|u| {
                let tir = self.reify_expr(u, ctx, None);
                vec![TirStmt::new(crate::tir::TirStmtKind::Expr(tir), u.span())]
            })
            .unwrap_or_default()
    }

    /// Reify a let-chain (`if let PAT = e [&& BOOL]* { … }`) into
    /// nested Match / If stmts. Mirrors
    /// `Elaborator::resolve_let_chain_stmts` (stmt.rs:1099+).
    /// Shared by the `LetChain` branches of `reify_if_expr`,
    /// `reify_if_stmt`, and `reify_while`.
    ///
    /// Each `Let` element becomes a two-arm Match: the recorded
    /// pattern arm continues the chain via recursion, the
    /// wildcard arm falls back to the chain's `else_block`. Each
    /// `Expr` element becomes a single-branch `If` whose body is
    /// the recursive continuation. The recursion terminates at
    /// an empty element list, where the `then_block` is reified
    /// directly.
    fn reify_let_chain_stmts(
        &mut self,
        elements: &[ast::ConditionElement],
        then_block_ast: &ast::Block,
        else_block: Option<&crate::tir::TirBlock>,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        span: crate::token::Span,
    ) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind, TypeTable};

        if elements.is_empty() {
            return self.reify_block(then_block_ast, ctx, expected_type).stmts;
        }

        match &elements[0] {
            ast::ConditionElement::Let {
                pattern,
                expr,
                span: elem_span,
            } => {
                let scrutinee = self.reify_expr(expr, ctx, None);
                let scrutinee_type = scrutinee.type_id;
                let tir_pattern = self.reify_pattern(pattern, scrutinee_type, ctx);
                let inner_stmts = self.reify_let_chain_stmts(
                    &elements[1..],
                    then_block_ast,
                    else_block,
                    ctx,
                    expected_type,
                    span,
                );
                let inner_block = TirBlock::new(inner_stmts, span);
                // Use the shared `block_result_type` (mirroring
                // `resolve_let_chain_stmts` stmt.rs:1140) so a then/else
                // block ending in a value `If` / `Match` / nested chain
                // contributes its real result type. A hand-rolled
                // "last stmt is Expr" check would mis-classify those
                // trailing forms as `Unit`, collapsing the Match's
                // `match_type` to `Unit` and dropping the branch values.
                let then_type = crate::tir::block_result_type(&inner_block);
                let else_tir = else_block.cloned();
                let else_type = else_tir
                    .as_ref()
                    .map_or(TypeTable::UNIT, crate::tir::block_result_type);
                let else_arm_span = else_tir.as_ref().map_or(span, |b| b.span);
                let match_type =
                    crate::tir::agree_branch_types(then_type, else_type).unwrap_or(TypeTable::UNIT);
                let then_body = TirExpr::new(TirExprKind::Block(inner_block), then_type, span);
                let else_body = match else_tir {
                    Some(b) => {
                        let b_span = b.span;
                        TirExpr::new(TirExprKind::Block(b), else_type, b_span)
                    }
                    None => TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                };
                let arms = vec![
                    TirMatchArm {
                        pattern: tir_pattern,
                        guard: None,
                        body: then_body,
                        span: *elem_span,
                    },
                    TirMatchArm {
                        pattern: TirPattern::Wildcard,
                        guard: None,
                        body: else_body,
                        span: else_arm_span,
                    },
                ];
                vec![TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Match {
                            expr: Box::new(scrutinee),
                            arms,
                        },
                        match_type,
                        span,
                    )),
                    span,
                )]
            }
            ast::ConditionElement::Expr(expr) => {
                let condition = self.reify_expr(expr, ctx, Some(TypeTable::BOOL));
                let inner_stmts = self.reify_let_chain_stmts(
                    &elements[1..],
                    then_block_ast,
                    else_block,
                    ctx,
                    expected_type,
                    span,
                );
                let inner_block = TirBlock::new(inner_stmts, span);
                vec![TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block: inner_block,
                        else_block: else_block.cloned(),
                    },
                    span,
                )]
            }
        }
    }

    /// Reify a trailing stmt-position `if` whose value flows out as the
    /// enclosing block's result. Mirrors
    /// `Elaborator::resolve_if_stmt_with_expected` (stmt.rs:1042): the
    /// `LetChain` arm reuses the let-chain lowering with `expected_type`
    /// threaded through so the chain's then/else blocks stay
    /// value-producing; the `Expr` arm emits an `If` *expression*
    /// statement (not a value-dropping stmt `If`) so the branch values
    /// become the block result.
    fn reify_if_stmt_with_expected(
        &mut self,
        if_stmt: &ast::IfStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> Vec<TirStmt> {
        use crate::tir::{TirExprKind, TirStmtKind, TypeTable};
        match &if_stmt.condition {
            ast::Condition::LetChain { elements, .. } => {
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, expected_type));
                ctx.enter_scope();
                let stmts = self.reify_let_chain_stmts(
                    elements,
                    &if_stmt.then_block,
                    else_block.as_ref(),
                    ctx,
                    expected_type,
                    if_stmt.span,
                );
                ctx.exit_scope();
                stmts
            }
            ast::Condition::Expr(cond_expr) => {
                let condition = self.reify_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let then_branch = self.reify_block(&if_stmt.then_block, ctx, expected_type);
                let else_branch = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, expected_type));
                let then_type = crate::tir::block_result_type(&then_branch);
                let else_type = else_branch
                    .as_ref()
                    .map_or(TypeTable::UNIT, crate::tir::block_result_type);
                let result_type =
                    crate::tir::agree_branch_types(then_type, else_type).unwrap_or(TypeTable::UNIT);
                let if_expr = TirExpr::new(
                    TirExprKind::If {
                        condition: Box::new(condition),
                        then_branch,
                        else_branch,
                    },
                    result_type,
                    if_stmt.span,
                );
                vec![TirStmt::new(TirStmtKind::Expr(if_expr), if_stmt.span)]
            }
        }
    }

    /// Reify a stmt-position `if cond { … } else { … }`. Stmt
    /// position never carries an `expected_type` from the surrounding
    /// block (the elaborator switches to `…_with_expected` only on a
    /// trailing position; reify follows suit by passing `None` to the
    /// branches). `Condition::LetChain` mirrors the expression-level
    /// `IfLetChain` desugar; the chain expansion lives behind the
    /// same `todo!` as `reify_if_expr`.
    fn reify_if_stmt(&mut self, if_stmt: &ast::IfStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::TirStmtKind;
        match &if_stmt.condition {
            ast::Condition::Expr(cond_expr) => {
                let condition = self.reify_expr(cond_expr, ctx, Some(crate::tir::TypeTable::BOOL));
                let then_block = self.reify_block(&if_stmt.then_block, ctx, None);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, None));
                vec![TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    },
                    if_stmt.span,
                )]
            }
            ast::Condition::LetChain { elements, .. } => {
                // Mirror `Elaborator::resolve_if_stmt`'s
                // `Condition::LetChain` arm (stmt.rs:1014+): the
                // chain elements lower into nested Match / If
                // stmts via the shared `reify_let_chain_stmts`.
                // Else-branch resolves in the outer scope (chain
                // bindings aren't visible there); the chain body
                // gets its own scope.
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, None));
                ctx.enter_scope();
                let stmts = self.reify_let_chain_stmts(
                    elements,
                    &if_stmt.then_block,
                    else_block.as_ref(),
                    ctx,
                    None,
                    if_stmt.span,
                );
                ctx.exit_scope();
                stmts
            }
        }
    }

    /// Reify an `if cond { … } else { … }` expression. The `cond`
    /// shape is restricted to `Condition::Expr` here; `Condition::LetChain`
    /// dispatches through the `IfLetChain` desugar (`sem.types.desugars`
    /// records the tag at annotate time — `Elaborator::resolve_if_expr`
    /// at expr.rs:1860). The chain expansion needs `let`-binding +
    /// branch-merging logic that mirrors `resolve_let_chain_stmts` and
    /// is deferred to a follow-up.
    fn reify_if_expr(
        &mut self,
        if_expr: &ast::IfExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        recorded_type: TypeId,
    ) -> TirExpr {
        let cond_expr = match &if_expr.condition {
            ast::Condition::Expr(e) => e,
            ast::Condition::LetChain { elements, .. } => {
                // Mirror `Elaborator::resolve_if_expr`'s
                // `Condition::LetChain` arm (expr.rs:1867+): the
                // chain reduces to a `Block` of nested Match /
                // If stmts via `reify_let_chain_stmts`. The
                // overall block's result type is the recorded
                // `expected_type` (or `recorded_type` as a
                // fallback when no expectation propagated).
                let else_block = if_expr
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, expected_type));
                ctx.enter_scope();
                let stmts = self.reify_let_chain_stmts(
                    elements,
                    &if_expr.then_block,
                    else_block.as_ref(),
                    ctx,
                    expected_type,
                    if_expr.span,
                );
                ctx.exit_scope();
                let chain_block = crate::tir::TirBlock::new(stmts, if_expr.span);
                return TirExpr::new(
                    crate::tir::TirExprKind::Block(chain_block),
                    recorded_type,
                    if_expr.span,
                );
            }
        };
        let condition = self.reify_expr(cond_expr, ctx, Some(crate::tir::TypeTable::BOOL));
        let then_branch = self.reify_block(&if_expr.then_block, ctx, expected_type);
        let else_branch = if_expr
            .else_block
            .as_ref()
            .map(|b| self.reify_block(b, ctx, expected_type));
        TirExpr::new(
            crate::tir::TirExprKind::If {
                condition: Box::new(condition),
                then_branch,
                else_branch,
            },
            recorded_type,
            if_expr.span,
        )
    }

    /// Reify a binary expression. When the elaborator dispatched the
    /// operator to a trait method (Gap 11), the
    /// `sem.types.operator_dispatch[binary.id]` entry carries the
    /// `(FunctionRef, self_kind, arg_ref_wraps, return_type)` reify
    /// needs to emit the same `TirExprKind::MethodCall` shape. Absence
    /// of an entry means the elaborator emitted a native
    /// `TirExprKind::Binary`; reify mirrors with the 1:1 op mapping.
    fn reify_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, ResolvedType, TirBinaryOp, TirExprKind, TirUnaryOp, TypeTable};

        // Mirror `resolve_binary_operands_with_coercion` (operators.rs:80):
        // a numeric-literal operand is typed from the *other* operand (or,
        // when both are literals, from the expression's recorded type). This
        // matters for inlined associated-const bodies like
        // `f32::INFINITY = 1.0 / 0.0`, whose literals carry no recorded type
        // of their own — without the hint they default to `f64` and the
        // surrounding arithmetic lowers to the wrong width / an integer op.
        let left_is_lit = self.tysys.is_numeric_literal(&binary.left);
        let right_is_lit = self.tysys.is_numeric_literal(&binary.right);
        let (left, right) = if left_is_lit && !right_is_lit {
            let right = self.reify_expr(&binary.right, ctx, None);
            let coerce = if self.tysys.type_table.borrow().is_numeric(right.type_id) {
                Some(right.type_id)
            } else {
                None
            };
            let left = self.reify_expr(&binary.left, ctx, coerce);
            (left, right)
        } else if right_is_lit && !left_is_lit {
            let left = self.reify_expr(&binary.left, ctx, None);
            let coerce = if self.tysys.type_table.borrow().is_numeric(left.type_id) {
                Some(left.type_id)
            } else {
                None
            };
            let right = self.reify_expr(&binary.right, ctx, coerce);
            (left, right)
        } else if left_is_lit && right_is_lit {
            // Both literals: use the expression's recorded type as the hint
            // (e.g. the `const_ty` flowing in from a reified const body).
            let hint = if recorded_type == TypeTable::UNKNOWN {
                None
            } else {
                Some(recorded_type)
            };
            let left = self.reify_expr(&binary.left, ctx, hint);
            let right = self.reify_expr(&binary.right, ctx, hint);
            (left, right)
        } else {
            let left = self.reify_expr(&binary.left, ctx, None);
            let right = self.reify_expr(&binary.right, ctx, None);
            (left, right)
        };

        // Reference equality: when both operands are references, the
        // elaborator emits `RefEq` / `RefNotEq` (identity comparison)
        // rather than dispatching to `Eq` — and records no operator
        // dispatch. The decision is from operand types alone
        // (operators.rs:150), so reify reproduces it here.
        if matches!(binary.op, ast::BinaryOp::Eq | ast::BinaryOp::NotEq) {
            let both_refs = {
                let tt = self.tysys.type_table.borrow();
                matches!(
                    (tt.get(left.type_id), tt.get(right.type_id)),
                    (ResolvedType::Ref(_), ResolvedType::Ref(_))
                        | (ResolvedType::MutRef(_), ResolvedType::MutRef(_))
                )
            };
            if both_refs {
                let op = if binary.op == ast::BinaryOp::Eq {
                    TirBinaryOp::RefEq
                } else {
                    TirBinaryOp::RefNotEq
                };
                return TirExpr::new(
                    TirExprKind::Binary {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    TypeTable::BOOL,
                    binary.span,
                );
            }
        }

        if let Some(dispatch) = self.ann_operator_dispatch(binary.id) {
            // Operator-trait dispatch path. Reuse the shared receiver
            // adjuster (statically; no Elaborator needed) and the
            // shared arg-wrap helper to produce TIR identical to what
            // `build_trait_op_method_call_on_resolved` emitted.
            let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                left,
                dispatch.self_kind,
                /* is_ref_impl */ false,
                binary.span,
                &self.tysys.type_table,
            );
            let args = vec![right];
            let call_args: Vec<CallArg> = args
                .into_iter()
                .zip(dispatch.arg_ref_wraps.iter().copied())
                .map(|(arg, wrap)| {
                    let arg_expr = if wrap {
                        let arg_ref_type = self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(arg.type_id));
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(arg),
                            },
                            arg_ref_type,
                            binary.span,
                        )
                    } else {
                        arg
                    };
                    CallArg::new(arg_expr, false)
                })
                .collect();
            let call = super::Elaborator::<H>::build_tir_method_call(
                receiver,
                dispatch.function_ref,
                vec![],
                call_args,
                dispatch.return_type,
                binary.span,
            );
            // Comparison operators dispatch to `Eq::eq` / `Ord::cmp` but
            // the source operator decides the wrapping the elaborator
            // applies after the call: `!=` negates the `eq` result, and
            // `<` / `>` / `<=` / `>=` compare the `cmp` `Ordering` against
            // the variant that makes the operator true.
            return match binary.op {
                ast::BinaryOp::NotEq if call.type_id == TypeTable::BOOL => TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(call),
                    },
                    TypeTable::BOOL,
                    binary.span,
                ),
                ast::BinaryOp::Lt
                | ast::BinaryOp::Gt
                | ast::BinaryOp::LtEq
                | ast::BinaryOp::GtEq
                    if call.type_id != TypeTable::ERROR =>
                {
                    super::operators::ord_bool_from_cmp(
                        call,
                        binary.op,
                        binary.span,
                        &self.tysys.type_table,
                    )
                }
                _ => call,
            };
        }

        // Native binary op — primitive path. The op mapping is 1:1
        // with the AST. Stage 5 follow-up: ref-equality
        // (`RefEq` / `RefNotEq`) is synthesised by the elaborator
        // after type analysis; until that decision is recorded, reify
        // emits the source-level op verbatim. The Cap on this is the
        // `==` / `!=` path on ref types; other ops on refs would
        // already be diagnosed by annotate.
        TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(left),
                op: ast_binary_op_to_tir(binary.op),
                right: Box::new(right),
            },
            recorded_type,
            binary.span,
        )
    }

    /// Reify a template string `"…{expr}…"`. Mirrors
    /// `Elaborator::resolve_template_string` (template.rs:16+):
    /// - Constant fast path: no interpolations → concatenate the
    ///   string parts at reify time and emit a `StringLiteral`.
    /// - Single-`String`-typed interpolation with no format spec →
    ///   forward the resolved expression unchanged.
    /// - General case: build a `Vec<TirTemplatePart>` where each
    ///   `Interpolation` part carries an optional
    ///   [`crate::tir::TemplateFormatSpec`] parsed by
    ///   [`super::template::parse_format_spec`].
    fn reify_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
        _recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirTemplatePart};

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        let span = template.span;

        let has_interpolation = template
            .parts
            .iter()
            .any(|p| matches!(p, ast::TemplatePart::Interpolation { .. }));

        if !has_interpolation {
            let mut combined = String::new();
            for part in &template.parts {
                if let ast::TemplatePart::String(s) = part {
                    let unescaped = super::util::unescape_template_string(s).unwrap_or_default();
                    combined.push_str(&unescaped);
                }
            }
            return TirExpr::new(TirExprKind::StringLiteral(combined), string_type, span);
        }

        if template.parts.len() == 1
            && let ast::TemplatePart::Interpolation { expr, format: None } = &template.parts[0]
        {
            let resolved = self.reify_expr(expr, ctx, None);
            if resolved.type_id == string_type {
                return resolved;
            }
        }

        let mut parts = Vec::new();
        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    if !s.is_empty() {
                        let unescaped =
                            super::util::unescape_template_string(s).unwrap_or_default();
                        if !unescaped.is_empty() {
                            parts.push(TirTemplatePart::Literal(unescaped));
                        }
                    }
                }
                ast::TemplatePart::Interpolation { expr, format } => {
                    let resolved = self.reify_expr(expr, ctx, None);
                    let format_spec = format
                        .as_ref()
                        .map(|f| super::template::parse_format_spec(&f.spec));
                    parts.push(TirTemplatePart::Interpolation {
                        expr: Box::new(resolved),
                        format_spec,
                    });
                }
            }
        }

        TirExpr::new(TirExprKind::TemplateString { parts }, string_type, span)
    }

    /// Reify a `a..<b` / `a..=b` range expression. The elaborator
    /// lowers ranges into the prelude's `RangeExclusive` /
    /// `RangeInclusive` struct literals (expr.rs:4397+); reify
    /// produces the same shape by reading the element type from
    /// the reified `start` expression and interning the
    /// `GenericInstance` via `make_generic_instance`.
    fn reify_range(
        &mut self,
        range: &ast::RangeExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::ast::RangeKind;
        use crate::tir::{TirExprKind, TirStructField, TypeTable};

        // Resolve both operands first; the element type comes from
        // `start` (annotate has unified start/end to the same type, so
        // either operand's type works).
        let start = self.reify_expr(&range.start, ctx, None);
        let end_expected = Some(start.type_id);
        let end = self.reify_expr(&range.end, ctx, end_expected);
        let element_type = start.type_id;

        // The recorded `expression_types[range.id]` carries the
        // assembled `GenericInstance` type, but the elaborator's
        // construction is purely from the prelude's compiler-item
        // registry — reproduce here so the same `module_source` lands
        // even if a future inference change made the recorded type
        // less specific.
        let (struct_name, module_source) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            match range.kind {
                RangeKind::Exclusive => (
                    "RangeExclusive".to_string(),
                    items
                        .struct_module(crate::compiler_item::CompilerItem::RangeExclusive)
                        .cloned()
                        .unwrap_or_else(crate::module_source::ModuleSource::range),
                ),
                RangeKind::Inclusive => (
                    "RangeInclusive".to_string(),
                    items
                        .struct_module(crate::compiler_item::CompilerItem::RangeInclusive)
                        .cloned()
                        .unwrap_or_else(crate::module_source::ModuleSource::range),
                ),
            }
        };

        let struct_type = self.tysys.type_table.borrow_mut().make_generic_instance(
            struct_name.clone(),
            module_source,
            vec![element_type],
        );

        let mut fields = vec![
            TirStructField {
                name: "start".to_string(),
                value: start,
                field_index: 0,
            },
            TirStructField {
                name: "end".to_string(),
                value: end,
                field_index: 1,
            },
        ];
        if matches!(range.kind, RangeKind::Inclusive) {
            fields.push(TirStructField {
                name: "exhausted".to_string(),
                value: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, range.span),
                field_index: 2,
            });
        }

        let arg_names = vec![self.tysys.type_table.borrow().type_name(element_type)];
        let mangled_name = crate::name::mangle_generic_name(&struct_name, &arg_names);

        // Honour the recorded result type if present (annotate may
        // have unified with a more specific `RangeInclusive<i32>` etc.
        // already on `recorded_type`); reify trusts it as the final
        // expression type.
        let _ = recorded_type;

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_name,
                fields,
            },
            struct_type,
            range.span,
        )
    }

    /// Reify a named `StructLiteralExpr`. Field types come from
    /// `tysys.all_struct_fields`; the instance type + `type_args` for
    /// generic structs come from Gap 1's
    /// `sem.types.generic_instantiations[id]` record. Anonymous
    /// struct literals (`{ x: 1, y: 2 }` with no leading type name)
    /// flow through a different elaborator helper and are deferred to
    /// a follow-up.
    fn reify_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStructField};

        // `KeyValueLiteralBuilder` coercion: when the elaborator
        // recorded `key_value_coercions[struct_lit.id]`, the literal
        // was lowered through `Builder::new_literal` /
        // `Builder::insert_literal` / `Builder::build`. Reify replays
        // the same `__kv_lit:` desugar block deterministically.
        if let Some(facts) = self.ann_key_value_coercions(struct_lit.id) {
            return self.reify_key_value_coercion(struct_lit, facts, ctx, struct_lit.span);
        }

        let Some(struct_name) = struct_lit.name.clone() else {
            // Anonymous struct literal `{ x: 1, y: 2 }` — annotate
            // synthesised the struct from the field shape and
            // registered it via `make_struct` + populated
            // `local_struct_fields` / `pending_anonymous_structs`.
            // Reify reproduces the deterministic naming scheme and
            // reads the already-registered type back from the type
            // table.
            return self.reify_anonymous_struct_literal(struct_lit, ctx, recorded_type);
        };

        // Field positional info from the decl-interned struct.
        let lookup = self.type_lookup();
        // Decl field shape: (name, index, raw_type, default_expr).
        // Cloned out of the lookup so the borrow ends before reifying.
        let decl_fields: Vec<(String, u32, TypeId, Option<ast::Expr>)> = {
            let info = lookup.struct_fields(&struct_name);
            info.map(|info| {
                info.fields
                    .iter()
                    .enumerate()
                    .map(|(i, (n, t, _is_pub))| {
                        let default = info.field_defaults.get(i).and_then(Option::clone);
                        (n.clone(), i as u32, *t, default)
                    })
                    .collect()
            })
            .unwrap_or_default()
        };
        let field_names_to_index: crate::hashmap::IndexMap<String, (u32, TypeId)> = decl_fields
            .iter()
            .map(|(n, i, t, _)| (n.clone(), (*i, *t)))
            .collect();

        // Instance type for generic structs is recorded by Gap 1; for
        // non-generic structs Gap 1's recording is skipped and we use
        // the bare struct type from `recorded_type`.
        let (struct_type, generic_args): (TypeId, Vec<TypeId>) = self
            .ann_generic_instantiations(struct_lit.id)
            .map(|gi| (gi.instance_type, gi.type_args))
            .unwrap_or((recorded_type, Vec::new()));

        let mangled_struct_name = if generic_args.is_empty() {
            struct_name
        } else {
            let arg_names: Vec<String> = generic_args
                .iter()
                .map(|&t| self.tysys.type_table.borrow().type_name(t))
                .collect();
            crate::name::mangle_generic_name(&struct_name, &arg_names)
        };

        // Substitute the decl's `TypeParam`s with the instance's generic
        // args so a field's expected type is concrete (a no-op for
        // non-generic structs, where `generic_args` is empty).
        let substitute = |this: &Self, raw: TypeId| -> TypeId {
            if generic_args.is_empty() {
                return raw;
            }
            let subst: crate::hashmap::IndexMap<u32, TypeId> = (0..generic_args.len() as u32)
                .zip(generic_args.iter().copied())
                .collect();
            this.tysys
                .type_table
                .borrow_mut()
                .substitute_type_params(raw, &subst)
        };

        // Reify each AST-provided field, then synthesize omitted fields
        // that declared a default (`port: i32 = 8080`). Field order in
        // the TIR is by declaration index — matching
        // `Elaborator::resolve_struct_literal`, which sorts after
        // filling defaults so codegen's positional slots line up.
        let mut fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .map(|f| {
                let (field_index, raw_ty) = field_names_to_index
                    .get(&f.name)
                    .copied()
                    .unwrap_or((0, crate::tir::TypeTable::UNKNOWN));
                let expected_field_ty = substitute(self, raw_ty);
                let value = self.reify_expr(&f.value, ctx, Some(expected_field_ty));
                TirStructField {
                    name: f.name.clone(),
                    value,
                    field_index,
                }
            })
            .collect();

        let provided: crate::hashmap::IndexSet<String> =
            struct_lit.fields.iter().map(|f| f.name.clone()).collect();
        for (name, field_index, raw_ty, default) in &decl_fields {
            if provided.contains(name) {
                continue;
            }
            if let Some(default_expr) = default {
                let expected_field_ty = substitute(self, *raw_ty);
                let value = self.reify_expr(default_expr, ctx, Some(expected_field_ty));
                fields.push(TirStructField {
                    name: name.clone(),
                    value,
                    field_index: *field_index,
                });
            }
        }
        fields.sort_by_key(|f| f.field_index);

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_struct_name,
                fields,
            },
            struct_type,
            struct_lit.span,
        )
    }

    /// Reify a compound assignment `x += y` / `x -= y` / etc. The
    /// elaborator desugars to `x = x op y` and routes through
    /// `assign_to_target`, which handles complex lvalues
    /// (`a[i] += x` etc.). Reify handles the common case: target is
    /// any expression that produces a writeable place; the desugared
    /// shape is `Assign { target, value: Binary { left: target, op,
    /// right: value } }`. The shared `reify_binary` path picks the
    /// native vs operator-trait dispatch for the inner op via Gap 11.
    fn reify_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::ast::CompoundAssignOp;
        use crate::tir::{TirExprKind, TypeTable};

        let op = match compound.op {
            CompoundAssignOp::Add => crate::tir::TirBinaryOp::Add,
            CompoundAssignOp::Sub => crate::tir::TirBinaryOp::Sub,
            CompoundAssignOp::Mul => crate::tir::TirBinaryOp::Mul,
            CompoundAssignOp::Div => crate::tir::TirBinaryOp::Div,
            CompoundAssignOp::Mod => crate::tir::TirBinaryOp::Mod,
            CompoundAssignOp::BitAnd => crate::tir::TirBinaryOp::BitAnd,
            CompoundAssignOp::BitOr => crate::tir::TirBinaryOp::BitOr,
            CompoundAssignOp::BitXor => crate::tir::TirBinaryOp::BitXor,
            CompoundAssignOp::Shl => crate::tir::TirBinaryOp::Shl,
            CompoundAssignOp::Shr => crate::tir::TirBinaryOp::Shr,
        };

        // The target appears twice in the desugared shape (as the
        // read for the binary op, and as the assignment target). The
        // elaborator side reifies it once and emits the same node
        // twice (it's pure for the lvalue shapes the elaborator
        // accepts); reify mirrors by walking the AST twice.
        let read = self.reify_expr(&compound.target, ctx, None);
        let rhs = self.reify_expr(&compound.value, ctx, Some(read.type_id));
        let combined_type = read.type_id;
        // Operator-overloaded operands (`u128 /= u128`, …): the combined
        // value dispatches through the trait method (`Div::div`), recorded by
        // `resolve_compound_assign` under the compound's AstId. Replay that
        // MethodCall — a raw `Binary` with a primitive `/` on struct operands
        // would lower to invalid Wasm. Mirrors the `reify_binary` dispatch
        // path (keyed on `binary.id`).
        let combined = if let Some(dispatch) = self.ann_operator_dispatch(compound.id) {
            let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                read,
                dispatch.self_kind,
                /* is_ref_impl */ false,
                compound.span,
                &self.tysys.type_table,
            );
            let call_args: Vec<crate::tir::CallArg> = std::iter::once(rhs)
                .zip(dispatch.arg_ref_wraps.iter().copied())
                .map(|(arg, wrap)| {
                    let arg_expr = if wrap {
                        let arg_ref_type = self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(crate::tir::ResolvedType::Ref(arg.type_id));
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: crate::tir::TirUnaryOp::Ref,
                                expr: Box::new(arg),
                            },
                            arg_ref_type,
                            compound.span,
                        )
                    } else {
                        arg
                    };
                    crate::tir::CallArg::new(arg_expr, false)
                })
                .collect();
            super::Elaborator::<H>::build_tir_method_call(
                receiver,
                dispatch.function_ref,
                vec![],
                call_args,
                dispatch.return_type,
                compound.span,
            )
        } else {
            TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(read),
                    op,
                    right: Box::new(rhs),
                },
                combined_type,
                compound.span,
            )
        };

        // IndexAssign rewrite for `arr[i] OP= v`: dispatch the
        // assignment side through `index_assign_dispatch` so reify
        // emits `arr.index_assign(i, combined)` (the same MethodCall
        // production's `assign_to_target` builds), not a plain
        // `Assign` whose target is an `Index` expression.
        if let ast::Expr::Index(index_expr) = &compound.target
            && let Some(dispatch) = self.ann_index_assign_dispatch(index_expr.id)
        {
            let receiver = self.reify_expr(&index_expr.expr, ctx, None);
            let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                receiver,
                dispatch.self_kind,
                false,
                compound.span,
                &self.tysys.type_table,
            );
            let idx_expr = self.reify_expr(&index_expr.index, ctx, None);
            let _ = recorded_type;
            return super::Elaborator::<H>::build_tir_method_call(
                receiver,
                dispatch.function_ref,
                vec![],
                vec![
                    crate::tir::CallArg::new(idx_expr, false),
                    crate::tir::CallArg::new(combined, false),
                ],
                dispatch.return_type,
                compound.span,
            );
        }

        // Re-walk the target for the assignment side. For the simple
        // local / global / field-access cases this reproduces the same
        // TIR shape; IndexMut targets (`a[i] OP= x` where the trait
        // resolves to `IndexMut` not `IndexAssign`) remain a follow-up.
        let target_for_assign = self.reify_expr(&compound.target, ctx, None);
        let _ = recorded_type;
        // Global-var compound-assign: `g OP= v` lowers to
        // `GlobalVarSet { value: g OP v }` so codegen actually
        // mutates the global. Mirrors production's
        // `assign_to_target` (operators.rs:1192+) which the
        // compound-assign desugar also feeds through.
        if let TirExprKind::GlobalVarGet {
            module_source,
            name,
        } = &target_for_assign.kind
        {
            return TirExpr::new(
                TirExprKind::GlobalVarSet {
                    module_source: module_source.clone(),
                    name: name.clone(),
                    value: Box::new(combined),
                },
                TypeTable::UNIT,
                compound.span,
            );
        }
        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target_for_assign),
                value: Box::new(combined),
            },
            TypeTable::UNIT,
            compound.span,
        )
    }

    /// Reify the `?` postfix operator. The elaborator desugars
    /// `expr?` based on the operand's type:
    /// - `Option<T>`: `match expr { Some(v) => v, None => return null }`
    /// - `Result<T, E>`: `match expr { Ok(v) => v, Err(e) =>
    ///   return Err(From::from(e)) }`
    ///
    /// The annotate-side validation (operand is `Option` / `Result`,
    /// function return type is compatible) has already fired, so
    /// reify trusts the recorded shape and produces the matching
    /// `Match` TIR.
    fn reify_question_mark(
        &mut self,
        qm: &ast::TryOpExpr,
        ctx: &mut FunctionContext,
        _recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{ResolvedType, TirExprKind, TypeTable};

        let inner = self.reify_expr(&qm.expr, ctx, None);
        let inner_type = inner.type_id;

        let (is_option, is_result) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.as_option(inner_type).is_some(),
                matches!(
                    tt.get(inner_type),
                    ResolvedType::GenericInstance { name, .. } if name == "Result"
                ),
            )
        };

        if is_option {
            self.reify_question_mark_option(inner, ctx, qm.span)
        } else if is_result {
            self.reify_question_mark_result(inner, ctx, qm.span)
        } else {
            // Annotate already diagnosed; produce a Unit-typed
            // placeholder of `ERROR` so downstream phases see the
            // same shape annotate's recovery path produced.
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span)
        }
    }

    /// `Option<T>`'s `?`-op desugar — mirrors
    /// `Elaborator::resolve_question_mark_option` (expr.rs:4000+).
    fn reify_question_mark_option(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind, TypeTable};

        let inner_type = inner.type_id;
        let (some_type, some_name, none_name) = {
            let tt = self.tysys.type_table.borrow();
            let some_type = tt.as_option(inner_type).unwrap();
            let items = tt.compiler_items();
            (
                some_type,
                items
                    .variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
                    .to_string(),
                items
                    .variant_case_name(crate::compiler_item::CompilerItem::OptionNone)
                    .to_string(),
            )
        };

        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), some_type, false, None);

        let some_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: some_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: some_type,
                }],
                payload_type: some_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                some_type,
                span,
            ),
            span,
        };

        let none_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: none_name,
                bindings: vec![],
                payload_type: TypeTable::UNIT,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(TirExpr::new(TirExprKind::Null, inner_type, span)),
                        },
                        span,
                    )],
                    span,
                )),
                TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![some_arm, none_arm],
            },
            some_type,
            span,
        )
    }

    /// `Result<T, E>`'s `?`-op desugar — mirrors
    /// `Elaborator::resolve_question_mark_result` (expr.rs:4087+).
    /// The `From::from` synthesis path on mismatched error types is
    /// staged for a Stage 5 follow-up; the same-error-type case
    /// (most common in fixtures) lands here.
    fn reify_question_mark_result(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{
            ResolvedType, TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind,
        };

        let inner_type = inner.type_id;
        let return_type = ctx.return_type;

        let (ok_type, inner_err_type) = match self.tysys.type_table.borrow().get(inner_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => panic!("reify_question_mark_result: ? operand must be Result<T, E>"),
        };
        let outer_err_type = match self.tysys.type_table.borrow().get(return_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => type_args[1],
            _ => panic!("reify_question_mark_result: ? return type must be Result<U, F>"),
        };

        // When inner and outer error types differ, synthesise a
        // `<OuterErr>::from(<InnerErr>_val)` call. Mirrors
        // `Elaborator::resolve_from_call` (expr.rs:4263+); the
        // module source for the impl is looked up via the same
        // search annotate runs (walk impl blocks across loaded
        // modules to find a matching `impl From<InnerErr> for
        // OuterErr`).
        let need_from_conversion = inner_err_type != outer_err_type;

        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), ok_type, false, None);
        let e_local = ctx.add_local("__qm_e".to_string(), inner_err_type, false, None);

        let (ok_name, err_name, err_index) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            let (_, _, ok_n, _ok_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
            let (_, _, err_n, err_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
            (ok_n.to_string(), err_n.to_string(), err_i)
        };

        let ok_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: ok_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: ok_type,
                }],
                payload_type: ok_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                ok_type,
                span,
            ),
            span,
        };

        let e_expr = TirExpr::new(
            TirExprKind::Local {
                index: e_local,
                name: "__qm_e".to_string(),
            },
            inner_err_type,
            span,
        );
        let converted_err = if need_from_conversion {
            self.reify_from_call(outer_err_type, inner_err_type, e_expr, span)
        } else {
            e_expr
        };
        let err_variant = TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: return_type,
                case_index: err_index,
                case_name: err_name.clone(),
                payload: Some(Box::new(converted_err)),
            },
            return_type,
            span,
        );

        let err_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: err_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_e".to_string(),
                    local_index: e_local,
                    type_id: inner_err_type,
                }],
                payload_type: inner_err_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(err_variant),
                        },
                        span,
                    )],
                    span,
                )),
                crate::tir::TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![ok_arm, err_arm],
            },
            ok_type,
            span,
        )
    }

    /// Reify a comparison chain `a < b < c …`. Mirrors
    /// `Elaborator::desugar_comparison_chain` (operators.rs:1313+):
    /// each middle term `m_k` binds to a `__m{k}` local so it is
    /// not re-evaluated, and the chain reduces to
    /// `(a < m_0) && (m_0 < m_1) && … && (m_{n-1} < tail)` wrapped
    /// in a block that holds the `__mK` bindings.
    ///
    /// Native primitive comparisons emit `TirExprKind::Binary`
    /// directly; non-primitive operands route through trait
    /// dispatch in the elaborator. The trait-dispatch path inside
    /// the chain is staged for a Stage 5 follow-up: the synthesised
    /// inner comparisons have no source AST id, so Gap 11's
    /// `operator_dispatch` record (keyed by `AstId`) doesn't catch
    /// them. The primitive-only path covers the common fixture
    /// shape (`x < y && y < z`); non-primitive chains fall to the
    /// recovery shape (native Binary on whatever types the operands
    /// land at, plus the elaborator's `RequiresTrait` diagnostic
    /// will have already fired on the annotate side).
    fn reify_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirBinaryOp, TirBlock, TirExprKind, TirStmtKind, TypeTable};

        if chain.comparisons.is_empty() {
            // Degenerate parse — annotate emits `chain.first` as-is.
            return self.reify_expr(&chain.first, ctx, None);
        }

        if chain.comparisons.len() == 1 {
            let cmp = &chain.comparisons[0];
            let left = self.reify_expr(&chain.first, ctx, None);
            let right = self.reify_expr(&cmp.right, ctx, Some(left.type_id));

            // Non-primitive comparison dispatches through `Eq::eq` /
            // `Ord::cmp`; the recording fires on `chain.id` at
            // operators.rs:1346.
            if let Some(dispatch) = self.ann_operator_dispatch(chain.id) {
                let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                    left,
                    dispatch.self_kind,
                    /* is_ref_impl */ false,
                    chain.span,
                    &self.tysys.type_table,
                );
                let args = vec![right];
                let call_args: Vec<crate::tir::CallArg> = args
                    .into_iter()
                    .zip(dispatch.arg_ref_wraps.iter().copied())
                    .map(|(arg, wrap)| {
                        let arg_expr = if wrap {
                            let arg_ref_type = self
                                .tysys
                                .type_table
                                .borrow_mut()
                                .intern(crate::tir::ResolvedType::Ref(arg.type_id));
                            TirExpr::new(
                                TirExprKind::Unary {
                                    op: crate::tir::TirUnaryOp::Ref,
                                    expr: Box::new(arg),
                                },
                                arg_ref_type,
                                chain.span,
                            )
                        } else {
                            arg
                        };
                        crate::tir::CallArg::new(arg_expr, false)
                    })
                    .collect();
                let method_call = super::Elaborator::<H>::build_tir_method_call(
                    receiver,
                    dispatch.function_ref,
                    vec![],
                    call_args,
                    dispatch.return_type,
                    chain.span,
                );

                // Ord ops wrap `cmp(...) ==/!= Less/Greater`;
                // `!=` via `Eq::eq` wraps with `!`.
                use ast::BinaryOp;
                if matches!(
                    cmp.op,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
                ) {
                    return self.wrap_ord_bool_from_cmp(method_call, cmp.op, chain.span);
                }
                if cmp.op == BinaryOp::NotEq && method_call.type_id == TypeTable::BOOL {
                    return TirExpr::new(
                        TirExprKind::Unary {
                            op: crate::tir::TirUnaryOp::Not,
                            expr: Box::new(method_call),
                        },
                        TypeTable::BOOL,
                        chain.span,
                    );
                }
                return method_call;
            }

            let recorded_type = self
                .ann_expression_types(chain.id)
                .unwrap_or(TypeTable::BOOL);
            return TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(left),
                    op: ast_binary_op_to_tir(cmp.op),
                    right: Box::new(right),
                },
                recorded_type,
                cmp.op_span,
            );
        }

        ctx.enter_scope();
        let mut stmts: Vec<TirStmt> = Vec::new();

        let cmp0 = &chain.comparisons[0];
        let first_tir = self.reify_expr(&chain.first, ctx, None);
        let right0_tir = self.reify_expr(&cmp0.right, ctx, Some(first_tir.type_id));

        // Bind first middle to `__m0`.
        let m0_type = right0_tir.type_id;
        let m0_name = "__m0".to_string();
        let m0_index = ctx.add_local(m0_name.clone(), m0_type, false, None);
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: m0_name.clone(),
                local_index: m0_index,
                is_mut: false,
                is_reactive: false,
                type_id: m0_type,
                value: right0_tir,
                skip_value_copy: false,
            },
            chain.span,
        ));
        let m0_ref = TirExpr::new(
            TirExprKind::Local {
                index: m0_index,
                name: m0_name,
            },
            m0_type,
            chain.span,
        );

        let mut acc_tir = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(first_tir),
                op: ast_binary_op_to_tir(cmp0.op),
                right: Box::new(m0_ref.clone()),
            },
            TypeTable::BOOL,
            cmp0.op_span,
        );
        let mut prev_tir = m0_ref;

        let last_idx = chain.comparisons.len() - 1;
        for idx in 1..chain.comparisons.len() {
            let cmp = &chain.comparisons[idx];
            let raw_right = self.reify_expr(&cmp.right, ctx, Some(prev_tir.type_id));
            let right_tir = if idx == last_idx {
                raw_right
            } else {
                let m_type = raw_right.type_id;
                let m_name = format!("__m{idx}");
                let m_index = ctx.add_local(m_name.clone(), m_type, false, None);
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: m_name.clone(),
                        local_index: m_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: m_type,
                        value: raw_right,
                        skip_value_copy: false,
                    },
                    chain.span,
                ));
                TirExpr::new(
                    TirExprKind::Local {
                        index: m_index,
                        name: m_name,
                    },
                    m_type,
                    chain.span,
                )
            };
            let next_prev = right_tir.clone();
            let cmp_tir = TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(prev_tir),
                    op: ast_binary_op_to_tir(cmp.op),
                    right: Box::new(right_tir),
                },
                TypeTable::BOOL,
                cmp.op_span,
            );
            acc_tir = TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(acc_tir),
                    op: TirBinaryOp::And,
                    right: Box::new(cmp_tir),
                },
                TypeTable::BOOL,
                chain.span,
            );
            prev_tir = next_prev;
        }

        ctx.exit_scope();

        stmts.push(TirStmt::new(TirStmtKind::Expr(acc_tir), chain.span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, chain.span)),
            TypeTable::BOOL,
            chain.span,
        )
    }

    /// Reify an `expr[idx]` index expression.
    ///
    /// Three shapes per `Elaborator::resolve_index` (expr.rs:1659+):
    /// - Tuple constant index → `TirExprKind::FieldAccess` with the
    ///   constant index as `field_index` / `field_name`.
    /// - `Index` trait dispatch → `*receiver.index(idx)` wrapped in
    ///   `Unary { Deref }`; the `operator_dispatch[index.id]` record
    ///   (Gap 11) carries the dispatch target. The `Ref(Output)`
    ///   `return_type` on the record is the signal that the outer
    ///   `Deref` wrap is needed.
    /// - `IndexValue` trait dispatch → `receiver.index_value(idx)`
    ///   returns the value by copy; no outer wrap.
    fn reify_index(
        &mut self,
        index: &ast::IndexExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, ResolvedType, TirExprKind, TirUnaryOp, TypeTable};

        let receiver = self.reify_expr(&index.expr, ctx, None);

        // Tuple constant-index path: detect via the receiver's
        // resolved type + the index being a constant integer
        // literal. Matches `Elaborator::resolve_index`'s tuple
        // branch (expr.rs:1674+).
        let tuple_elems: Option<Vec<TypeId>> = {
            let tt = self.tysys.type_table.borrow();
            let base = receiver.type_id;
            let unwrapped = match tt.get(base) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                _ => base,
            };
            tt.as_tuple(unwrapped)
        };
        if let Some(elems) = tuple_elems
            && let ast::Expr::Literal(lit) = &index.index
            && let ast::Literal::Number(repr) = &lit.value
            && let Ok(idx) = repr.parse::<usize>()
            && idx < elems.len()
        {
            return TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(receiver),
                    field_index: idx as u32,
                    field_name: idx.to_string(),
                },
                elems[idx],
                index.span,
            );
        }

        // Operator-dispatch path: Gap 11's `operator_dispatch[index.id]`
        // carries the resolved Index / IndexValue trait method. The
        // `return_type` on the record signals whether the outer
        // `Deref` wrap applies (Index returns `&Output`; IndexValue
        // returns `Output`).
        if let Some(dispatch) = self.ann_operator_dispatch(index.id) {
            let adjusted_receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                receiver,
                dispatch.self_kind,
                false,
                index.span,
                &self.tysys.type_table,
            );
            let idx_expr = self.reify_expr(&index.index, ctx, None);
            let method_call = super::Elaborator::<H>::build_tir_method_call(
                adjusted_receiver,
                dispatch.function_ref,
                vec![],
                vec![CallArg::new(idx_expr, false)],
                dispatch.return_type,
                index.span,
            );
            // `Index` trait returns `&Output`, so the outer wrap is a
            // `Deref` (`expr[i]` → `*expr.index(i)`). Annotate records
            // this explicitly: a return-type-shape check would misfire
            // for an `IndexValue` whose `Output` is itself a reference
            // (`Array<&i32>::index_value` → `&i32`) and double-deref.
            if dispatch.needs_deref {
                return TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: Box::new(method_call),
                    },
                    recorded_type,
                    index.span,
                );
            }
            return method_call;
        }

        // No dispatch recorded → the elaborator emitted a recovery
        // shape (annotate would have diagnosed missing trait impl).
        // Match the recovery output with a Unit placeholder typed
        // as ERROR.
        let _ = recorded_type;
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, index.span)
    }

    /// Reify a closure expression. The Gap 4 record
    /// (`sem.types.closure_captures[closure.id]`) carries the
    /// capture-analysis result annotate computed:
    /// - `mut_captures`: outer mut-locals to materialise as
    ///   `let __ref_v = &mut v;` before the closure body opens, in
    ///   declaration order.
    /// - `captures`: the closure's final capture list (name +
    ///   outer-index + type + mut flag).
    /// - `is_mutating`: drives the `fn mut(...)` vs `fn(...)` tag on
    ///   the closure type.
    ///
    /// Mirror `Elaborator::resolve_closure` (closure.rs:127+) step
    /// by step so the walk-order invariant (Gap 7) lands the
    /// same locals at the same indices.
    fn reify_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        use crate::tir::{
            ResolvedType, TirBlock, TirCapture, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable,
        };

        let span = closure.span;

        let cap_info = self.ann_closure_captures(closure.id).unwrap_or_else(|| {
            super::sem::types::ClosureCaptureInfo {
                mut_captures: Vec::new(),
                captures: Vec::new(),
                is_mutating: false,
            }
        });

        // Step 1 (replay): materialise outer-scope `__ref_v` locals
        // for each mut-capture in the recorded order; emit the
        // matching `let __ref_v = &mut v;` TIR; register
        // `deref_overrides` so the closure body's references to
        // captured mut-locals dereference the proxy.
        let mut ref_stmts: Vec<TirStmt> = Vec::new();
        let mut deref_overrides: crate::hashmap::IndexMap<String, (String, TypeId)> =
            crate::hashmap::IndexMap::default();
        for mc in &cap_info.mut_captures {
            ctx.add_local(mc.ref_name.clone(), mc.ref_type, false, None);
            ctx.address_taken_locals.insert(mc.outer_index);
            ref_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: mc.ref_name.clone(),
                    local_index: ctx.next_local - 1,
                    is_mut: false,
                    is_reactive: false,
                    type_id: mc.ref_type,
                    value: TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: mc.outer_index,
                                    name: mc.var_name.clone(),
                                },
                                mc.inner_type,
                                span,
                            )),
                        },
                        mc.ref_type,
                        span,
                    ),
                    skip_value_copy: false,
                },
                span,
            ));
            deref_overrides.insert(mc.var_name.clone(), (mc.ref_name.clone(), mc.inner_type));
        }

        // Step 2: open the closure context with the deref overrides.
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.tysys.type_table);
        closure_ctx.deref_overrides = deref_overrides;

        // Step 3: add closure parameters. Param types come from the
        // AST (resolved via the outer scope's type-param view); if
        // the expected fn type is available, prefer its param types
        // for cases where the closure has no explicit annotation.
        // Peel newtypes so a closure coerced to a `type Reducer = fn(..)`
        // newtype still sees the underlying function signature; otherwise
        // unannotated closure params (`|a, b| ...`) resolve to UNKNOWN and
        // the functor signature diverges from the call site, leaving the
        // `__call` method unreachable. Mirrors production's coercion-aware
        // param inference.
        let expected_fn_type = expected_type.map(|t| {
            let table = self.tysys.type_table.borrow();
            table.get_ultimate_base_type(table.peel_refs(t))
        });
        let expected_fn_params: Option<Vec<TypeId>> =
            expected_fn_type.and_then(|t| match self.tysys.type_table.borrow().get(t) {
                ResolvedType::Function { params, .. } => Some(params.clone()),
                _ => None,
            });
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let type_id = if let Some(ty) = &p.ty {
                    self.resolve_type(ty)
                } else if let Some(ref fn_params) = expected_fn_params {
                    fn_params.get(i).copied().unwrap_or(TypeTable::UNKNOWN)
                } else {
                    TypeTable::UNKNOWN
                };
                closure_ctx.add_local(p.name.clone(), type_id, p.is_mut, Some(p.id));
                (p.name.clone(), type_id)
            })
            .collect();

        // Step 4: reify the body in the closure scope.
        let body_expected =
            expected_fn_type.and_then(|t| match self.tysys.type_table.borrow().get(t) {
                ResolvedType::Function { return_type, .. } => Some(*return_type),
                _ => None,
            });
        let body = self.reify_expr(&closure.body, &mut closure_ctx, body_expected);

        // Step 5: assemble the capture list from the recorded entries.
        let captures: Vec<TirCapture> = cap_info
            .captures
            .iter()
            .map(|c| TirCapture {
                name: c.name.clone(),
                outer_index: c.outer_index,
                type_id: c.type_id,
                is_mut: c.is_mut,
            })
            .collect();

        // Block bodies with explicit `return X` have a NEVER/UNIT
        // tail; the closure's logical return is the returned type.
        // Production: closure.rs:276+.
        let return_type = if let TirExprKind::Block(ref block) = body.kind {
            super::Elaborator::<H>::find_return_type_in_block(block).unwrap_or(body.type_id)
        } else {
            body.type_id
        };

        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.tysys.type_table.borrow_mut().make_function_with_mut(
            cap_info.is_mutating,
            param_types,
            return_type,
            Vec::new(),
            Vec::new(),
        );

        let mut all_locals = closure_ctx.locals;
        let body_locals = if params.len() <= all_locals.len() {
            all_locals.split_off(params.len())
        } else {
            Vec::new()
        };
        let address_taken_locals = closure_ctx.address_taken_locals;

        let declared_effects =
            expected_type.and_then(|t| match self.tysys.type_table.borrow().get(t) {
                ResolvedType::Function { effects, .. } if !effects.is_empty() => {
                    Some(effects.clone())
                }
                _ => None,
            });

        let closure_tir = TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
                functor_id: None,
                address_taken_locals,
                body_locals,
                declared_effects,
            },
            func_type,
            span,
        );

        // Step 7: wrap in a Block when ref_stmts materialised any
        // outer-scope `__ref_v` bindings.
        if ref_stmts.is_empty() {
            let _ = recorded_type;
            return closure_tir;
        }

        let mut stmts = ref_stmts;
        stmts.push(TirStmt::new(TirStmtKind::Expr(closure_tir), span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, span)),
            func_type,
            span,
        )
    }

    /// Synthesise a `<TargetType>::from(<value>)` call for the `?`
    /// operator's error-conversion path. Mirrors
    /// `Elaborator::resolve_from_call` (expr.rs:4263+): builds a
    /// mangled `__<Target>__From<From>__from` method name with the
    /// `LocalMethodName` carrying the canonical From-impl
    /// Reify a tuple-to-sequence coercion (`[1, 2, 3]: Array<i32>`).
    /// Mirrors `try_coerce_tuple_to_sequence_inner`'s desugar block
    /// shape (`__seq_lit: { let __b = Builder::new_literal(N); __b.push_literal(...); ...; break __seq_lit: __b.build(); }`).
    /// The Stage-5 walk-order invariant (Gap 7) keeps the `__b`
    /// local at the same `FunctionContext` index reify reserves for
    /// it, so the resulting TIR is byte-identical to production's.
    /// True when `type_id` is a `TypePack` or a tuple whose elements
    /// transitively contain a `TypePack`. Mirrors
    /// `Elaborator::type_contains_pack` (expr.rs:3752).
    fn type_contains_pack(&self, type_id: TypeId) -> bool {
        use crate::tir::{ResolvedType, TypeTable};
        let ty = self.tysys.type_table.borrow().get(type_id).clone();
        match ty {
            ResolvedType::TypePack { .. } => true,
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } if TypeTable::is_tuple_type(&name, &module_source) => {
                type_args.iter().any(|e| self.type_contains_pack(*e))
            }
            _ => false,
        }
    }

    /// Reify a tuple literal, handling spread elements (`[..rest, b]`,
    /// `[a, ..middle, b]`). Mirrors `Elaborator::resolve_tuple_literal`
    /// (expr.rs:3768): the tuple `TypeId` is built bottom-up from the
    /// resolved element types via `make_tuple` so a nested tuple's element
    /// type is the identical interned id as the inner literal's own type
    /// (avoiding the `nir/sroa` `TypeId`-identity divergence at `-O2`).
    /// Spread elements expand per `type_contains_pack`:
    /// - a direct `TypePack` → `TypePackExpansion`,
    /// - a tuple containing a pack → `TupleSpread` (monomorphize expands),
    /// - a concrete tuple → inline `FieldAccess` per element (binding
    ///   non-trivial spread operands to a `__spread_N` temporary).
    fn reify_tuple_literal(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{
            ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeTable,
        };

        let mut elements: Vec<TirExpr> = Vec::new();
        let mut elem_types: Vec<TypeId> = Vec::new();
        // (local_idx, name, expr, span) for non-trivial spread operands.
        let mut spread_bindings: Vec<(u32, String, TirExpr, crate::token::Span)> = Vec::new();

        for elem in &tuple_lit.elements {
            if let ast::Expr::Spread(inner, _span) = elem {
                let spread_expr = self.reify_expr(inner, ctx, None);
                let contains_pack = self.type_contains_pack(spread_expr.type_id);
                let spread_type = self
                    .tysys
                    .type_table
                    .borrow()
                    .get(spread_expr.type_id)
                    .clone();
                if contains_pack {
                    let is_direct_pack = matches!(
                        self.tysys.type_table.borrow().get(spread_expr.type_id),
                        ResolvedType::TypePack { .. }
                    );
                    if is_direct_pack {
                        let pack_type_id = spread_expr.type_id;
                        elem_types.push(spread_expr.type_id);
                        elements.push(TirExpr::new(
                            TirExprKind::TypePackExpansion {
                                call_expr: Box::new(spread_expr),
                                pack_type_id,
                            },
                            *elem_types.last().unwrap(),
                            elem.span(),
                        ));
                    } else {
                        elem_types.push(spread_expr.type_id);
                        elements.push(TirExpr::new(
                            TirExprKind::TupleSpread {
                                expr: Box::new(spread_expr),
                            },
                            *elem_types.last().unwrap(),
                            elem.span(),
                        ));
                    }
                } else if let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args: inner_elems,
                } = spread_type
                    && TypeTable::is_tuple_type(&name, &module_source)
                {
                    // Concrete tuple: expand inline via FieldAccess. Bind a
                    // non-trivial operand to a temporary for single evaluation.
                    let spread_ref = if matches!(spread_expr.kind, TirExprKind::Local { .. }) {
                        spread_expr
                    } else {
                        let spread_type_id = spread_expr.type_id;
                        let tmp_name = format!("__spread_{}", ctx.next_local);
                        let tmp_idx = ctx.add_local(tmp_name.clone(), spread_type_id, false, None);
                        spread_bindings.push((tmp_idx, tmp_name.clone(), spread_expr, elem.span()));
                        TirExpr::new(
                            TirExprKind::Local {
                                index: tmp_idx,
                                name: tmp_name,
                            },
                            spread_type_id,
                            elem.span(),
                        )
                    };
                    for (i, &et) in inner_elems.iter().enumerate() {
                        elements.push(TirExpr::new(
                            TirExprKind::FieldAccess {
                                expr: Box::new(spread_ref.clone()),
                                field_index: i as u32,
                                field_name: i.to_string(),
                            },
                            et,
                            elem.span(),
                        ));
                        elem_types.push(et);
                    }
                } else {
                    // A stray spread of a non-tuple — annotate already
                    // diagnosed it; pass the operand through unchanged.
                    elem_types.push(spread_expr.type_id);
                    elements.push(spread_expr);
                }
            } else {
                let resolved = self.reify_expr(elem, ctx, None);
                elem_types.push(resolved.type_id);
                elements.push(resolved);
            }
        }

        let tuple_type = self.tysys.type_table.borrow_mut().make_tuple(elem_types);
        let tuple_expr = TirExpr::new(TirExprKind::TupleLiteral { elements }, tuple_type, span);

        if spread_bindings.is_empty() {
            tuple_expr
        } else {
            let mut stmts: Vec<TirStmt> = spread_bindings
                .into_iter()
                .map(|(idx, name, value, span)| {
                    let type_id = value.type_id;
                    TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index: idx,
                            value,
                            is_mut: false,
                            is_reactive: false,
                            type_id,
                            skip_value_copy: false,
                        },
                        span,
                    )
                })
                .collect();
            stmts.push(TirStmt::new(TirStmtKind::Expr(tuple_expr), span));
            let block = TirBlock::new(stmts, span);
            TirExpr::new(TirExprKind::Block(block), tuple_type, span)
        }
    }

    fn reify_sequence_coercion(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        facts: super::sem::types::SequenceCoercionFacts,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::name::{LocalMethodName, MethodName};
        use crate::tir::{
            CallArg, FunctionRef, MonomorphInfo, TirBlock, TirExprKind, TirStmt, TirStmtKind,
            TypeTable,
        };

        let label = "__seq_lit".to_string();
        ctx.enter_scope();

        // --- Builder::new_literal(capacity) ---
        let new_method_info = LocalMethodName::new(
            facts.builder_base_name.clone(),
            Some(facts.trait_name.clone()),
            "new_literal".to_string(),
        )
        .with_struct_type_args(&facts.type_arg_names);
        let new_mangled_name = MethodName::format_local(
            &facts.mangled_builder_name,
            Some(&facts.trait_name),
            "new_literal",
        );
        let capacity = tuple_lit.elements.len() as u64;
        let new_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: facts.impl_module_source.clone(),
                    name: new_mangled_name,
                    monomorph_info: if facts.type_arg_ids.is_empty() {
                        None
                    } else {
                        Some(MonomorphInfo {
                            generic_name: format!("{}::new_literal", facts.builder_base_name),
                            impl_type_args: facts.type_arg_ids.clone(),
                            method_type_args: vec![],
                            is_blanket: false,
                        })
                    },
                    method_info: Some(new_method_info),
                },
                type_args: vec![],
                args: vec![CallArg::new(
                    TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: capacity,
                            repr: (capacity as i64).to_string(),
                        },
                        TypeTable::I32,
                        span,
                    ),
                    false,
                )],
            },
            facts.builder_type,
            span,
        );

        let builder_index = ctx.add_local("__b".to_string(), facts.builder_type, true, None);
        let mut stmts = vec![TirStmt::new(
            TirStmtKind::Let {
                name: "__b".to_string(),
                local_index: builder_index,
                is_mut: true,
                is_reactive: false,
                type_id: facts.builder_type,
                value: new_call,
                skip_value_copy: false,
            },
            span,
        )];

        // --- For each element: __b.push_literal(elem) ---
        let push_mangled_name = MethodName::format_local(
            &facts.mangled_builder_name,
            Some(&facts.trait_name),
            "push_literal",
        );
        let push_method_info = LocalMethodName::new(
            facts.builder_base_name.clone(),
            Some(facts.trait_name.clone()),
            "push_literal".to_string(),
        )
        .with_struct_type_args(&facts.type_arg_names);

        for element in &tuple_lit.elements {
            let elem_expr = self.reify_expr(element, ctx, Some(facts.element_type));
            let builder_local = TirExpr::new(
                TirExprKind::Local {
                    index: builder_index,
                    name: "__b".to_string(),
                },
                facts.builder_type,
                span,
            );
            let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                builder_local,
                facts.push_self_kind,
                false,
                span,
                &self.tysys.type_table,
            );
            let push_call = super::Elaborator::<H>::build_tir_method_call(
                receiver,
                FunctionRef {
                    module_source: facts.impl_module_source.clone(),
                    name: push_mangled_name.clone(),
                    monomorph_info: if facts.type_arg_ids.is_empty() {
                        None
                    } else {
                        Some(MonomorphInfo {
                            generic_name: format!("{}::push_literal", facts.builder_base_name),
                            impl_type_args: facts.type_arg_ids.clone(),
                            method_type_args: vec![],
                            is_blanket: false,
                        })
                    },
                    method_info: Some(push_method_info.clone()),
                },
                vec![],
                vec![CallArg::new(elem_expr, false)],
                TypeTable::UNIT,
                span,
            );
            stmts.push(TirStmt::new(TirStmtKind::Expr(push_call), span));
        }

        // --- break __seq_lit: __b.build(); ---
        let builder_local_final = TirExpr::new(
            TirExprKind::Local {
                index: builder_index,
                name: "__b".to_string(),
            },
            facts.builder_type,
            span,
        );
        let build_mangled_name = MethodName::format_local(
            &facts.mangled_builder_name,
            Some(&facts.trait_name),
            "build",
        );
        let build_method_info = LocalMethodName::new(
            facts.builder_base_name.clone(),
            Some(facts.trait_name.clone()),
            "build".to_string(),
        )
        .with_struct_type_args(&facts.type_arg_names);
        let build_call = super::Elaborator::<H>::build_tir_method_call(
            builder_local_final,
            FunctionRef {
                module_source: facts.impl_module_source.clone(),
                name: build_mangled_name,
                monomorph_info: if facts.type_arg_ids.is_empty() {
                    None
                } else {
                    Some(MonomorphInfo {
                        generic_name: format!("{}::build", facts.builder_base_name),
                        impl_type_args: facts.type_arg_ids.clone(),
                        method_type_args: vec![],
                        is_blanket: false,
                    })
                },
                method_info: Some(build_method_info),
            },
            vec![],
            vec![],
            facts.output_type,
            span,
        );

        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(build_call),
            },
            span,
        ));

        ctx.exit_scope();

        let block_expr = TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: facts.output_type,
            },
            facts.output_type,
            span,
        );

        if let Some(target_type) = facts.newtype_cast_to {
            TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(block_expr),
                    target_type,
                },
                target_type,
                span,
            )
        } else {
            block_expr
        }
    }

    /// Reify an anonymous-struct-to-map coercion. Mirrors
    /// `try_coerce_struct_to_map_inner`: `__kv_lit: { let __b =
    /// Builder::new_literal([N]); __b.insert_literal("k", v); ...;
    /// break __kv_lit: __b.build() / __b; }`. Walk-order invariant
    /// keeps `__b` at the same `FunctionContext` index reify
    /// reserved.
    fn reify_key_value_coercion(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        facts: super::sem::types::KeyValueCoercionFacts,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::name::{LocalMethodName, MethodName};
        use crate::tir::{
            CallArg, FunctionRef, MonomorphInfo, TirBlock, TirExprKind, TirStmt, TirStmtKind,
            TypeTable,
        };

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);

        let label = "__kv_lit".to_string();
        ctx.enter_scope();

        // --- Builder::new_literal([capacity]) ---
        let new_method_info = LocalMethodName::new(
            facts.builder_base_name.clone(),
            Some(facts.trait_name.clone()),
            "new_literal".to_string(),
        )
        .with_struct_type_args(&facts.type_arg_names);
        let new_mangled_name = MethodName::format_local(
            &facts.mangled_builder_name,
            Some(&facts.trait_name),
            "new_literal",
        );
        let capacity = struct_lit.fields.len() as u64;
        let new_args = if facts.use_new_api {
            vec![CallArg::new(
                TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: capacity,
                        repr: (capacity as i64).to_string(),
                    },
                    TypeTable::I32,
                    span,
                ),
                false,
            )]
        } else {
            vec![]
        };
        let new_call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: facts.impl_module_source.clone(),
                    name: new_mangled_name,
                    monomorph_info: if facts.type_arg_ids.is_empty() {
                        None
                    } else {
                        Some(MonomorphInfo {
                            generic_name: format!("{}::new_literal", facts.builder_base_name),
                            impl_type_args: facts.type_arg_ids.clone(),
                            method_type_args: vec![],
                            is_blanket: false,
                        })
                    },
                    method_info: Some(new_method_info),
                },
                type_args: vec![],
                args: new_args,
            },
            facts.builder_type,
            span,
        );

        let builder_index = ctx.add_local("__b".to_string(), facts.builder_type, true, None);
        let mut stmts = vec![TirStmt::new(
            TirStmtKind::Let {
                name: "__b".to_string(),
                local_index: builder_index,
                is_mut: true,
                is_reactive: false,
                type_id: facts.builder_type,
                value: new_call,
                skip_value_copy: false,
            },
            span,
        )];

        // --- For each field: __b.insert_literal("name", value) ---
        let insert_mangled_name = MethodName::format_local(
            &facts.mangled_builder_name,
            Some(&facts.trait_name),
            "insert_literal",
        );
        let insert_method_info = LocalMethodName::new(
            facts.builder_base_name.clone(),
            Some(facts.trait_name.clone()),
            "insert_literal".to_string(),
        );

        for field in &struct_lit.fields {
            let value = self.reify_expr(&field.value, ctx, Some(facts.value_type));
            let builder_local = TirExpr::new(
                TirExprKind::Local {
                    index: builder_index,
                    name: "__b".to_string(),
                },
                facts.builder_type,
                span,
            );
            let receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
                builder_local,
                facts.insert_self_kind,
                false,
                span,
                &self.tysys.type_table,
            );
            let key_expr = TirExpr::new(
                TirExprKind::StringLiteral(field.name.clone()),
                string_type,
                span,
            );
            let insert_call = super::Elaborator::<H>::build_tir_method_call(
                receiver,
                FunctionRef {
                    module_source: facts.impl_module_source.clone(),
                    name: insert_mangled_name.clone(),
                    monomorph_info: None,
                    method_info: Some(insert_method_info.clone()),
                },
                vec![],
                vec![CallArg::new(key_expr, false), CallArg::new(value, false)],
                TypeTable::UNIT,
                span,
            );
            stmts.push(TirStmt::new(TirStmtKind::Expr(insert_call), span));
        }

        // --- break __kv_lit: __b.build() (new API) or __b (legacy) ---
        let builder_local_final = TirExpr::new(
            TirExprKind::Local {
                index: builder_index,
                name: "__b".to_string(),
            },
            facts.builder_type,
            span,
        );
        let result_expr = if facts.use_new_api {
            let build_mangled_name = MethodName::format_local(
                &facts.mangled_builder_name,
                Some(&facts.trait_name),
                "build",
            );
            let build_method_info = LocalMethodName::new(
                facts.builder_base_name.clone(),
                Some(facts.trait_name.clone()),
                "build".to_string(),
            );
            let build_monomorph = if facts.type_arg_ids.is_empty() {
                None
            } else {
                Some(MonomorphInfo {
                    generic_name: format!("{}::build", facts.builder_base_name),
                    impl_type_args: facts.type_arg_ids.clone(),
                    method_type_args: vec![],
                    is_blanket: false,
                })
            };
            super::Elaborator::<H>::build_tir_method_call(
                builder_local_final,
                FunctionRef {
                    module_source: facts.impl_module_source.clone(),
                    name: build_mangled_name,
                    monomorph_info: build_monomorph,
                    method_info: Some(build_method_info),
                },
                vec![],
                vec![],
                facts.target_type,
                span,
            )
        } else {
            builder_local_final
        };

        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(result_expr),
            },
            span,
        ));

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: facts.target_type,
            },
            facts.target_type,
            span,
        )
    }

    /// signature, and finds the impl's home module by walking the
    /// current module + loaded modules looking for a matching
    /// `impl From<From> for Target`.
    fn reify_from_call(
        &mut self,
        target_type: TypeId,
        from_type: TypeId,
        value: TirExpr,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::name::{LocalMethodName, MethodName};
        use crate::tir::{CallArg, FunctionRef, TirExprKind};

        let (target_name, from_name, from_trait_name) = {
            let tt = self.tysys.type_table.borrow();
            let target = tt.type_name(target_type);
            let from = tt.type_name(from_type);
            let trait_n = tt
                .compiler_items()
                .trait_name(crate::compiler_item::CompilerItem::From)
                .to_string();
            (target, from, trait_n)
        };

        let from_trait = format!("{from_trait_name}<{from_name}>");
        let method_name = MethodName::format_local(&target_name, Some(&from_trait), "from");
        let module_source = self.find_from_impl_module(&target_name, &from_name, &from_trait_name);

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source,
                    name: method_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName {
                        struct_name: target_name.clone(),
                        base_struct_name: target_name,
                        trait_name: Some(from_trait),
                        base_trait_name: Some(from_trait_name),
                        base_trait_module: None,
                        trait_type_args: vec![],
                        method_name: "from".to_string(),
                        method_type_args: vec![],
                        is_type_param_receiver: false,
                        is_ref_impl: false,
                        cm_name: None,
                    }),
                },
                type_args: vec![],
                args: vec![CallArg::new(value, false)],
            },
            target_type,
            span,
        )
    }

    /// Find the module that hosts `impl From<From> for Target`.
    /// Mirrors `Elaborator::find_from_impl_module` (expr.rs:4319+);
    /// walks current-module items + all loaded modules looking for
    /// a matching `impl_block`. Falls back to the current module
    /// when no impl is found (the synthesis path expects a
    /// late-bound impl; codegen produces the body).
    fn find_from_impl_module(
        &self,
        target_name: &str,
        from_name: &str,
        from_trait_name: &str,
    ) -> ModuleSource {
        let check_impl = |impl_block: &ast::ImplBlock| -> bool {
            let impl_target = ast_type_name_static(&impl_block.ty);
            if impl_target != target_name {
                return false;
            }
            let Some(trait_type) = &impl_block.trait_type else {
                return false;
            };
            let base = ast_type_name_static(trait_type);
            if base != from_trait_name {
                return false;
            }
            if let ast::Type::Generic(g) = trait_type
                && let Some(arg) = g.args.first()
            {
                return ast_type_name_static(arg) == from_name;
            }
            false
        };

        for item in self.current_module_items {
            if let ast::Item::Impl(impl_block) = item
                && check_impl(impl_block)
            {
                return self.current_module_source.clone();
            }
        }

        for (source, module) in self.loaded_modules {
            for item in &module.items {
                if let ast::Item::Impl(impl_block) = item
                    && check_impl(impl_block)
                {
                    return source.clone();
                }
            }
        }

        self.current_module_source.clone()
    }

    /// Reify a `with E => h, … do { body }` effect handler block.
    /// Mirrors `Elaborator::resolve_with_handler` (handlers.rs:37+).
    ///
    /// Each handler binding is one of:
    /// - Explicit `Effect => handler_expr` — reify the effect type
    ///   to its `(module, name)` canonical key and emit a
    ///   `TirHandlerBinding` with `EffectRef::Concrete`.
    /// - Bundled `handler_expr` (no `=>`) — staged for a follow-up
    ///   that records the handler type's implemented effect set
    ///   per binding so reify can enumerate them without
    ///   re-running `trait_env.implements_effect` lookups.
    fn reify_with_handler(
        &mut self,
        with_expr: &ast::WithHandlerExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::{EffectRef, TirExprKind, TirHandlerBinding, TypeTable};

        let mut bindings: Vec<TirHandlerBinding> = Vec::with_capacity(with_expr.handlers.len());
        for binding in &with_expr.handlers {
            // Gap 13: annotate recorded the binding's effect
            // enumeration on `sem.types.handler_bindings`. Reify
            // reifies the handler expression and stitches one
            // `TirHandlerBinding` per recorded effect entry.
            let binding_key =
                crate::symbol::SymbolKey::new(self.current_module_source.clone(), binding.id);
            let Some(facts) = self.sem.types.handler_bindings.get(&binding_key).cloned() else {
                // Annotate didn't record this binding — either it
                // bailed (diagnosed type) or the binding shape is
                // unsupported; skip to mirror the elaborator's
                // recovery.
                continue;
            };
            let handler = self.reify_expr(&binding.handler, ctx, None);
            for entry in &facts.effects {
                bindings.push(TirHandlerBinding {
                    effect: Some(EffectRef::Concrete {
                        name: entry.name.clone(),
                        module_source: entry.module_source.clone(),
                    }),
                    trait_type_args: entry.trait_type_args.clone(),
                    handler: handler.clone(),
                    handler_type: facts.handler_type,
                    span: binding.span,
                    bundle_group: facts.bundle_group,
                });
            }
        }

        ctx.enter_scope();
        let body = self.reify_block(&with_expr.body, ctx, None);
        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::WithHandler {
                bindings,
                body,
                result_type: TypeTable::UNIT,
            },
            TypeTable::UNIT,
            with_expr.span,
        )
    }

    /// Reify-side canonicalisation of a decl name through the
    /// current module's import context. Mirrors
    /// `Elaborator::canonical_decl_key`. Used by `reify_with_handler`
    /// to canonicalise an effect reference's `(module, name)` key.
    fn canonical_decl_key(&self, name: &str) -> (ModuleSource, String) {
        if let Some(src) = self.sem.imports.effect_sources.get(name) {
            let original = self
                .sem
                .imports
                .import_original_names
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_string());
            return (src.clone(), original);
        }
        if let Some(src) = self.sem.imports.imported_type_sources.get(name) {
            let original = self
                .sem
                .imports
                .import_original_names
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_string());
            return (src.clone(), original);
        }
        (self.current_module_source.clone(), name.to_string())
    }

    /// Reify a `matches!`-style expression: `scrutinee matches { PAT
    /// [if guard] }`. Desugars (tagged `DesugarKind::Matches` at
    /// annotate time) into a two-arm match: pattern → true, wildcard
    /// → false. Mirror `Elaborator::desugar_matches_expr`
    /// (matches.rs:25+).
    fn reify_matches(&mut self, m: &ast::MatchesExpr, ctx: &mut FunctionContext) -> TirExpr {
        use crate::tir::{TirExprKind, TirMatchArm, TirPattern, TypeTable};

        let scrutinee = self.reify_expr(&m.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        ctx.enter_scope();
        let pattern_tir = self.reify_pattern(&m.pattern, scrutinee_type, ctx);
        let arm_body = match &m.guard {
            Some(guard) => self.reify_expr(guard, ctx, Some(TypeTable::BOOL)),
            None => TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, m.span),
        };
        ctx.exit_scope();

        let arms = vec![
            TirMatchArm {
                pattern: pattern_tir,
                guard: None,
                body: arm_body,
                span: m.span,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, m.span),
                span: m.span,
            },
        ];

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            TypeTable::BOOL,
            m.span,
        )
    }

    /// Reify an anonymous struct literal `{ x: 1, y: 2 }`. Annotate
    /// synthesises the struct from the field shape, gives it a
    /// deterministic `__anon_{x:i32,y:i32}`-style name, and registers
    /// it on `tysys.type_table` + `sem.decls.local_struct_fields` +
    /// `sem.decls.pending_anonymous_structs`. Reify reproduces the
    /// same name from the reified field types and looks the struct
    /// type up; the registration already happened during annotate so
    /// reify is a pure read.
    fn reify_anonymous_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStructField};

        let resolved_fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let value = self.reify_expr(&field.value, ctx, None);
                TirStructField {
                    name: field.name.clone(),
                    value,
                    field_index: index as u32,
                }
            })
            .collect();

        // Read the registered type back from `expression_types` — the
        // deterministic name derived from reified field types can
        // diverge from annotate's (evaporated coercion wrappers).
        // Production registers it in expr.rs:3603+.
        let struct_type = recorded_type;
        let struct_name = self.tysys.type_table.borrow().type_name(struct_type);

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields: resolved_fields,
            },
            struct_type,
            struct_lit.span,
        )
    }

    /// Reify a `MatchExpr`. The scrutinee is walked; each arm enters
    /// its own scope, reifies the pattern (which adds bindings to
    /// `ctx`), reifies the optional guard at `Bool`, and reifies the
    /// body at the match's `expected_type`. The result `TypeId` is
    /// the recorded type — annotate already unified arm body types
    /// into it.
    fn reify_match_expr(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirMatchArm, TypeTable};

        let scrutinee = self.reify_expr(&match_expr.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| {
                ctx.enter_scope();
                let pattern = self.reify_pattern(&arm.pattern, scrutinee_type, ctx);
                let guard = arm
                    .guard
                    .as_ref()
                    .map(|g| self.reify_expr(g, ctx, Some(TypeTable::BOOL)));
                let body = self.reify_expr(&arm.body, ctx, expected_type);
                ctx.exit_scope();
                TirMatchArm {
                    pattern,
                    guard,
                    body,
                    span: arm.span,
                }
            })
            .collect();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            recorded_type,
            match_expr.span,
        )
    }

    /// Reify a `StaticMethodCallExpr` (AST shape:
    /// `Type::method(args)` with the type parsed as a `Type` node
    /// rather than a `Call { callee: Ident("Type::method") }`).
    /// Used for fully-qualified static method calls like
    /// `Stream<u8>::new()` where the target carries type args.
    ///
    /// Reify follows the same shape as the qualified-callee
    /// branch of `reify_call`: resolve the target type, derive the
    /// impl module from the resolved struct's `module_source`,
    /// build the mangled `__Type__method` `FunctionRef`. Type
    /// args come from the call's turbofish, else from Gap 1's
    /// `generic_instantiations` record.
    fn reify_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::name::{LocalMethodName, MethodName};
        use crate::tir::{CallArg, ResolvedType, TirExprKind, TypeId};

        // Reuse the static-method `FunctionRef` annotate resolved
        // (mangled name + `cm_name` for CM binding synthesis). reify's
        // own target-type resolution can lose these for imported / CM
        // generic targets (`Future::<T>::new`, `Result::<…>::Ok`),
        // collapsing the struct name to empty and emitting an
        // unresolvable `::new` call. Variant-constructor turbofish shapes
        // are not recorded here (annotate returns before the static-call
        // path), so they fall through to the variant detection below.
        if let Some(dispatch) = self.ann_static_method_dispatch(static_call.id) {
            let args: Vec<CallArg> = static_call
                .args
                .iter()
                .zip(
                    dispatch
                        .param_is_mut
                        .iter()
                        .copied()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(a, is_mut)| CallArg::new(self.reify_expr(a, ctx, None), is_mut))
                .collect();
            // Replay the production `Call`'s exact type args (method-level;
            // impl args ride along in `function_ref.monomorph_info`).
            return TirExpr::new(
                TirExprKind::Call {
                    type_args: dispatch.type_args,
                    func: dispatch.function_ref,
                    args,
                },
                recorded_type,
                static_call.span,
            );
        }

        let target_type_id = self.resolve_type(&static_call.target_type);
        let (struct_name, struct_module): (String, crate::module_source::ModuleSource) = {
            let tt = self.tysys.type_table.borrow();
            match tt.get(target_type_id).clone() {
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                }
                | ResolvedType::GenericInstance {
                    name,
                    module_source,
                    ..
                }
                | ResolvedType::Newtype {
                    name,
                    module_source,
                    ..
                }
                | ResolvedType::Variant {
                    name,
                    module_source,
                }
                | ResolvedType::Flags {
                    name,
                    module_source,
                }
                | ResolvedType::Enum {
                    name,
                    module_source,
                }
                | ResolvedType::Resource {
                    name,
                    module_source,
                }
                | ResolvedType::GenericResource {
                    name,
                    module_source,
                    ..
                } => (name, module_source),
                ResolvedType::Primitive(prim) => (
                    prim.as_str().to_string(),
                    crate::module_source::ModuleSource::primitive(),
                ),
                _ => (String::new(), self.current_module_source.clone()),
            }
        };

        // Variant constructor in turbofish form (`Option::<T>::Some(x)`,
        // `Result::<T, E>::Ok(v)`): the target type is a variant and the
        // method names one of its cases. Must beat the static-method
        // dispatch below, which would emit an unresolved `Option::Some`
        // call. The variant name + instance type come from the call's
        // recorded expression type (a `GenericInstance` / `Variant`) when
        // available — reify's own resolution of the turbofish target can
        // collapse to an empty name for imported / CM args — falling back
        // to the resolved target type.
        let (variant_name, variant_type, variant_type_args): (String, TypeId, Vec<TypeId>) = {
            let tt = self.tysys.type_table.borrow();
            match tt.get(recorded_type).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } => (name, recorded_type, type_args),
                ResolvedType::Variant { name, .. } => (name, recorded_type, Vec::new()),
                _ => {
                    let args = match tt.get(target_type_id).clone() {
                        ResolvedType::GenericInstance { type_args, .. } => type_args,
                        _ => Vec::new(),
                    };
                    (struct_name.clone(), target_type_id, args)
                }
            }
        };
        if let Some(variant_info) = self.type_lookup().variant_case(&variant_name).cloned()
            && let Some((case_index, case_data)) = variant_info
                .cases
                .iter()
                .enumerate()
                .find(|(_, c)| c.name == static_call.method)
                .map(|(i, c)| (i, c.clone()))
        {
            let payload_type = self.get_variant_case_payload_type(
                &variant_name,
                &static_call.method,
                &variant_type_args,
            );
            let payload = static_call
                .args
                .first()
                .map(|a| Box::new(self.reify_expr(a, ctx, Some(payload_type))));
            return TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type,
                    case_index: case_index as u32,
                    case_name: case_data.name,
                    payload,
                },
                variant_type,
                static_call.span,
            );
        }

        let mangled_method_name = MethodName::format_local(&struct_name, None, &static_call.method);

        let explicit_method_type_args: Vec<TypeId> = static_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        let type_args: Vec<TypeId> = if explicit_method_type_args.is_empty() {
            self.ann_generic_instantiations(static_call.id)
                .map(|gi| gi.type_args)
                .unwrap_or_default()
        } else {
            explicit_method_type_args
        };

        let args: Vec<CallArg> = static_call
            .args
            .iter()
            .map(|a| CallArg::new(self.reify_expr(a, ctx, None), false))
            .collect();

        let method_info = LocalMethodName::new(struct_name, None, static_call.method.clone());

        TirExpr::new(
            TirExprKind::Call {
                func: crate::tir::FunctionRef {
                    module_source: struct_module,
                    name: mangled_method_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                type_args,
                args,
            },
            recorded_type,
            static_call.span,
        )
    }

    /// Parameter `(name, default)` list of a free function in
    /// declaration order. Empty for unknown callees.
    fn lookup_free_func_params(
        &self,
        module_source: &ModuleSource,
        func_name: &str,
    ) -> Vec<(String, Option<ast::Expr>)> {
        let Some(idx_map) = self.tysys.loaded_module_func_indices.get(module_source) else {
            return Vec::new();
        };
        let Some(&idx) = idx_map.get(func_name) else {
            return Vec::new();
        };
        let items: &[Item] = if module_source == &self.current_module_source {
            self.current_module_items
        } else if let Some(m) = self.loaded_modules.get(module_source) {
            &m.items
        } else {
            return Vec::new();
        };
        if let Some(Item::Function(func)) = items.get(idx) {
            func.params
                .iter()
                .map(|p| (p.name.clone(), p.default.clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Pad missing trailing positional args with the callee's
    /// declared defaults. Mirrors `Elaborator::pad_args_with_defaults`
    /// (call.rs:1853+); only bare-ident free functions have defaults.
    fn reify_pad_args_with_defaults(
        &mut self,
        callee: &ast::Expr,
        call_args_ast: &[ast::Expr],
        args: &mut Vec<crate::tir::CallArg>,
        callee_module: &ModuleSource,
        callee_name: &str,
        ctx: &mut FunctionContext,
    ) {
        let ast::Expr::Ident(ident) = callee else {
            return;
        };
        if ident.name.contains("::") {
            return;
        }
        let func_params = self.lookup_free_func_params(callee_module, callee_name);
        if func_params.is_empty() || args.len() >= func_params.len() {
            return;
        }
        let mut subs: IndexMap<String, ast::Expr> = IndexMap::default();
        for (i, arg_ast) in call_args_ast.iter().enumerate() {
            if let Some((name, _)) = func_params.get(i) {
                subs.insert(name.clone(), arg_ast.clone());
            }
        }
        // A default expression resolves in the *callee's* lexical scope:
        // it may reference items private to the callee module that the
        // caller cannot see (`paint(c = DEFAULT_VALUE)` where
        // `DEFAULT_VALUE` is a callee-module-private global). Production
        // routes this through `default_scope_module`, consulted during
        // ident resolution (expr.rs:914). Reify's `reify_ident` reads
        // globals from `self.sem.decls.current_module_globals` keyed to the
        // module-context triple, so swap that triple to the callee module
        // around the default walk when the callee is a different, loaded
        // module. The caller's `ctx` (locals) stays — earlier positional
        // args were already AST-substituted into `subs`.
        let loaded = self.loaded_modules;
        let all_sem = self.all_module_semantics;
        let callee_ctx: Option<(&[Item], &ModuleSemantics)> =
            if callee_module == &self.current_module_source {
                None
            } else {
                match (loaded.get(callee_module), all_sem.get(callee_module)) {
                    (Some(m), Some(callee_sem)) => Some((m.items.as_slice(), callee_sem)),
                    _ => None,
                }
            };
        let saved = callee_ctx.map(|(items, callee_sem)| {
            (
                std::mem::replace(&mut self.current_module_source, callee_module.clone()),
                std::mem::replace(&mut self.current_module_items, items),
                std::mem::replace(&mut self.sem, callee_sem),
            )
        });

        for i in args.len()..func_params.len() {
            let (name, default_ast) = match func_params.get(i) {
                Some((n, Some(d))) => (n.clone(), d.clone()),
                _ => break,
            };
            let mut default_expr = default_ast;
            default_expr.substitute_idents(&subs);
            let resolved = self.reify_expr(&default_expr, ctx, None);
            args.push(crate::tir::CallArg::new(resolved, false));
            subs.insert(name, default_expr);
        }

        if let Some((src, items, sem)) = saved {
            self.current_module_source = src;
            self.current_module_items = items;
            self.sem = sem;
        }
    }

    /// Wrap `Ord::cmp` into a `bool`: `<` → `cmp == Less`, `>` →
    /// `cmp == Greater`, `<=` → `cmp != Greater`, `>=` → `cmp != Less`.
    /// Mirrors [`super::Elaborator::ord_bool_from_cmp`] (operators.rs:1605+).
    fn wrap_ord_bool_from_cmp(
        &mut self,
        cmp_call: TirExpr,
        op: ast::BinaryOp,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::compiler_item::CompilerItem;
        use crate::tir::{TirBinaryOp, TirExprKind, TypeTable};

        let ordering_type_id = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_enum(CompilerItem::Ordering);
        let (less_name, less_index, greater_name, greater_index) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            let (_, _, less_name, less_index) = items.require_enum_case(CompilerItem::OrderingLess);
            let (_, _, greater_name, greater_index) =
                items.require_enum_case(CompilerItem::OrderingGreater);
            (
                less_name.to_string(),
                less_index,
                greater_name.to_string(),
                greater_index,
            )
        };
        let (compare_op, case_name, case_index): (TirBinaryOp, String, u32) = match op {
            ast::BinaryOp::Lt => (TirBinaryOp::Eq, less_name, less_index),
            ast::BinaryOp::Gt => (TirBinaryOp::Eq, greater_name, greater_index),
            ast::BinaryOp::LtEq => (TirBinaryOp::NotEq, greater_name, greater_index),
            ast::BinaryOp::GtEq => (TirBinaryOp::NotEq, less_name, less_index),
            _ => unreachable!("wrap_ord_bool_from_cmp called with non-Ord op {:?}", op),
        };
        let ordering_variant = TirExpr::new(
            TirExprKind::EnumConstruct {
                enum_type: ordering_type_id,
                case_name,
                case_index,
            },
            ordering_type_id,
            span,
        );
        TirExpr::new(
            TirExprKind::Binary {
                op: compare_op,
                left: Box::new(cmp_call),
                right: Box::new(ordering_variant),
            },
            TypeTable::BOOL,
            span,
        )
    }

    /// Reify a `CallExpr`. Stage 5 covers the common shapes: bare-ident
    /// callees that resolve to a current-module or imported free
    /// function (`TirExprKind::Call`), and qualified-ident
    /// variant-constructor calls (`Some(x)`, `Result::Ok(v)`)
    /// emitted as `TirExprKind::VariantConstruct` with a payload.
    /// Closure-call, indirect-callee, static-method, qualified-enum,
    /// and qualified-flags shapes route through `todo!` until each
    /// branch is ported — `Elaborator::resolve_call` (call.rs:200+)
    /// is the source they mirror.
    /// AST-only full type name (generic args, refs included), matching
    /// `Elaborator::get_type_name_full` without needing `&self`. Used to
    /// compare a `From<Arg>` marker impl's type argument against a resolved
    /// `type_name`.
    fn ast_type_name_full(ty: &ast::Type) -> String {
        match ty {
            ast::Type::Named(named) => named.name.clone(),
            ast::Type::Generic(generic) => {
                let args: Vec<String> = generic.args.iter().map(Self::ast_type_name_full).collect();
                format!("{}<{}>", generic.name, args.join(", "))
            }
            ast::Type::Reference(inner) => format!("&{}", Self::ast_type_name_full(inner)),
            ast::Type::MutReference(inner) => format!("&mut {}", Self::ast_type_name_full(inner)),
            _ => super::Elaborator::<H>::get_type_name_static(ty),
        }
    }

    /// Locate the module providing a bodyless `impl From<arg_type> for
    /// target;` synthesis-request marker impl, scanning the current module
    /// then loaded modules. Returns `None` when no such marker impl exists
    /// (so the caller falls back to reflexive / newtype / regular handling).
    /// Mirrors `has_from_synthesis_request` + `find_from_impl_module`
    /// (in `method_call.rs` / `expr.rs`) but reads only the AST tables
    /// reify holds.
    fn find_from_synthesis_module(
        &self,
        target_name: &str,
        arg_type_name: &str,
    ) -> Option<crate::module_source::ModuleSource> {
        let from_trait_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .trait_name(crate::compiler_item::CompilerItem::From)
            .to_string();
        let matches = |impl_block: &ast::ImplBlock| -> bool {
            if !impl_block.is_synthesize_request {
                return false;
            }
            let Some(trait_type) = &impl_block.trait_type else {
                return false;
            };
            if super::Elaborator::<H>::get_type_name_static(&impl_block.ty) != target_name
                || super::Elaborator::<H>::get_type_name_static(trait_type) != from_trait_name
            {
                return false;
            }
            // Match the `From<Arg>` type argument by its full name (with
            // generic args, `&`, etc.) so `From<Vec<i32>>` etc. compare
            // correctly — mirroring `has_from_synthesis_request`'s use of
            // `get_type_name_full` rather than the base-name-only static form.
            matches!(trait_type, ast::Type::Generic(g) if g.args.len() == 1
                && Self::ast_type_name_full(&g.args[0]) == arg_type_name)
        };
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && matches(impl_block)
            {
                return Some(self.current_module_source.clone());
            }
        }
        for (source, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && matches(impl_block)
                {
                    return Some(source.clone());
                }
            }
        }
        None
    }

    fn reify_call(
        &mut self,
        call: &ast::CallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, TirExprKind};

        let span = call.span;

        // Variant-ctor (`Variant::Case(payload)`) must beat the
        // `static_method_dispatch` arm: annotate also records the
        // ctor at call.rs:1146+, but that shape would lower to a
        // `Call` against a function that doesn't exist.
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some(pos) = ident.name.find("::")
        {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];
            if !suffix.contains("::") {
                let lookup = self.type_lookup();
                if let Some(variant_info) = lookup.variant_case(prefix).cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == suffix)
                        .map(|(i, c)| (i, c.clone()))
                {
                    let variant_type = self
                        .ann_generic_instantiations(call.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    let payload = call.args.first().map(|arg_expr| {
                        Box::new(self.reify_expr(arg_expr, ctx, Some(case_data.payload)))
                    });
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload,
                        },
                        variant_type,
                        span,
                    );
                }
            } else if let Some(inner) = suffix.find("::")
                && let Some(ns_source) = self.sem.imports.namespace_imports.get(prefix).cloned()
            {
                // `ns::Type::Case(payload)` — a namespace-imported variant
                // constructor with a payload. The nullary form is handled
                // in `reify_ident`; the payload form parses as a `Call`.
                // The case lives in the namespace's variant table; the
                // instance type is the call's recorded expression type
                // (annotate resolved it).
                let type_name = &suffix[..inner];
                let case_name = &suffix[inner + 2..];
                if let Some(variant_info) = self
                    .tysys
                    .all_variant_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == case_name)
                        .map(|(i, c)| (i, c.clone()))
                {
                    let variant_type = self
                        .ann_generic_instantiations(call.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    let payload = call.args.first().map(|arg_expr| {
                        Box::new(self.reify_expr(arg_expr, ctx, Some(case_data.payload)))
                    });
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload,
                        },
                        variant_type,
                        span,
                    );
                }

                // `ns::Type::method(args)` — a namespace-imported static
                // method (e.g. `geo::Point::new(x, y)`). Not a variant
                // case, so build a static-method `Call` against the
                // namespace module's `Type::method`. Without this the
                // call reaches the recovery `ERROR` shape, its result type
                // is lost, and monomorphization prunes the entire
                // namespace module as unreachable.
                if self
                    .tysys
                    .all_struct_fields
                    .get(&ns_source)
                    .is_some_and(|m| m.contains_key(type_name))
                {
                    let mangled = crate::name::MethodName::format_local(type_name, None, case_name);
                    let type_args: Vec<TypeId> = if call.type_args.is_empty() {
                        self.ann_generic_instantiations(call.id)
                            .map(|gi| gi.type_args)
                            .unwrap_or_default()
                    } else {
                        call.type_args
                            .iter()
                            .map(|ty| self.resolve_type(ty))
                            .collect()
                    };
                    let arg_calls: Vec<CallArg> = call
                        .args
                        .iter()
                        .map(|a| CallArg::new(self.reify_expr(a, ctx, None), false))
                        .collect();
                    let method_info = crate::name::LocalMethodName::new(
                        type_name.to_string(),
                        None,
                        case_name.to_string(),
                    );
                    return TirExpr::new(
                        TirExprKind::Call {
                            func: crate::tir::FunctionRef {
                                module_source: ns_source,
                                name: mangled,
                                monomorph_info: None,
                                method_info: Some(method_info),
                            },
                            type_args,
                            args: arg_calls,
                        },
                        recorded_type,
                        span,
                    );
                }
            }
        }

        // Static-method / builtin dispatch (`Type::method(args)`,
        // `builtin::fn(args)`): the elaborator recorded the resolved
        // `FunctionRef` on `sem.types.static_method_dispatch` so reify
        // can reproduce the same TIR `Call` shape without re-running
        // impl lookup, mangled-name construction, or monomorph-info
        // shaping (none of which are tractable from the AST alone).
        if let Some(dispatch) = self.ann_static_method_dispatch(call.id) {
            // Forward per-argument expected types for closure args that have
            // an unannotated param, so `|a, b| ...` coerced to a `fn`-typed
            // (or `fn`-newtype) param infers its params; otherwise the
            // closure's params stay UNKNOWN and its functor `__call` is
            // generated with `unknown` param types and dropped before codegen.
            // Restricted to unannotated-param closures so we never override the
            // body-inferred effects of an effect-polymorphic closure (an
            // expected `fn() with E` whose `E` is a generic effect param would
            // otherwise pin `declared_effects` to the param instead of the
            // closure's actual effects).
            let call_param_types = self.ann_call_param_types(call.id);
            let mut arg_exprs: Vec<CallArg> = call
                .args
                .iter()
                .enumerate()
                .zip(
                    dispatch
                        .param_is_mut
                        .iter()
                        .copied()
                        .chain(std::iter::repeat(false)),
                )
                .map(|((i, a), is_mut)| {
                    let expected = if arg_is_unannotated_closure(a) {
                        call_param_types
                            .as_ref()
                            .and_then(|pts| pts.get(i).copied())
                    } else {
                        None
                    };
                    let arg = self.reify_expr(a, ctx, expected);
                    CallArg::new(arg, is_mut)
                })
                .collect();
            self.reify_pad_args_with_defaults(
                &call.callee,
                &call.args,
                &mut arg_exprs,
                &dispatch.function_ref.module_source,
                &dispatch.function_ref.name,
                ctx,
            );
            // Type args: replay exactly what the production builder put on
            // the `Call`. This already folds in any explicit turbofish and,
            // crucially, carries only the method-level type args — a generic
            // struct's impl type args live in `function_ref.monomorph_info`,
            // so re-deriving from `generic_instantiations` (which is the flat
            // impl+method list) would mangle `Container<i32>::make` as
            // `Container::make<i32>` and miss the monomorphized instance.
            return TirExpr::new(
                TirExprKind::Call {
                    type_args: dispatch.type_args,
                    func: dispatch.function_ref,
                    args: arg_exprs,
                },
                recorded_type,
                span,
            );
        }

        // `Type::from(x)` with no explicit `From` impl — reflexive and
        // newtype conversions. Production's `resolve_call` handles these
        // inline (call.rs:514-569) and records no `static_method_dispatch`,
        // tagging the reflexive case with `NewtypeFromCollapse`; reify must
        // reproduce the same three shapes (otherwise it falls through to an
        // unresolvable `Type::from` `Call`). Only reached when a user `From`
        // impl coexists, since that routes `from` through the static-call
        // path while the builtin reflexive/newtype conversion stays implicit.
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some(pos) = ident.name.find("::")
            && &ident.name[pos + 2..] == "from"
            && !ident.name[pos + 2..].contains("::")
            && call.args.len() == 1
        {
            let prefix = ident.name[..pos].to_string();
            let arg = self.reify_expr(&call.args[0], ctx, None);
            let arg_type = arg.type_id;
            let arg_type_name = self.tysys.type_table.borrow().type_name(arg_type);

            // Bodyless `impl From<X> for Type;` marker impl — production
            // synthesizes a `From::from` call inline (method_call.rs:1189 →
            // expr.rs:resolve_from_call) and records no dispatch. Reify
            // rebuilds the same trait-qualified `Call` so monomorphization
            // emits the synthesized conversion. Checked before reflexive /
            // newtype, matching production's order.
            if let Some(module_source) = self.find_from_synthesis_module(&prefix, &arg_type_name) {
                let from_trait_name = self
                    .tysys
                    .type_table
                    .borrow()
                    .compiler_items()
                    .trait_name(crate::compiler_item::CompilerItem::From)
                    .to_string();
                let from_trait = format!("{from_trait_name}<{arg_type_name}>");
                let name =
                    crate::name::MethodName::format_local(&prefix, Some(&from_trait), "from");
                return TirExpr::new(
                    TirExprKind::Call {
                        func: crate::tir::FunctionRef {
                            module_source,
                            name,
                            monomorph_info: None,
                            method_info: Some(crate::name::LocalMethodName {
                                struct_name: prefix.clone(),
                                base_struct_name: prefix.clone(),
                                trait_name: Some(from_trait),
                                base_trait_name: Some(from_trait_name),
                                base_trait_module: None,
                                trait_type_args: vec![],
                                method_name: "from".to_string(),
                                method_type_args: vec![],
                                is_type_param_receiver: false,
                                is_ref_impl: false,
                                cm_name: None,
                            }),
                        },
                        type_args: vec![],
                        args: vec![CallArg::new(arg, false)],
                    },
                    recorded_type,
                    span,
                );
            }

            // Reflexive: `T::from(T_val)` — identity, return the argument
            // (the `NewtypeFromCollapse` desugar, call.rs:526).
            if arg_type_name == prefix {
                return arg;
            }

            // Newtype→Base: `Base::from(Newtype_val)` where the arg is a
            // newtype over `Base` — lower to a `Cast` (call.rs:534).
            let base_of_arg = self.tysys.type_table.borrow().get_newtype_base(arg_type);
            if let Some(base_id) = base_of_arg
                && self.tysys.type_table.borrow().type_name(base_id) == prefix
            {
                return TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(arg),
                        target_type: base_id,
                    },
                    base_id,
                    span,
                );
            }

            // Base→Newtype: `Newtype::from(Base_val)` where `Newtype` is a
            // newtype over the arg's type — lower to a `Cast` (call.rs:549).
            if let Some(newtype_id) = self.type_lookup().newtype(&prefix)
                && let Some(base_id) = self.tysys.type_table.borrow().get_newtype_base(newtype_id)
                && self.tysys.type_table.borrow().type_name(base_id) == arg_type_name
            {
                return TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(arg),
                        target_type: newtype_id,
                    },
                    newtype_id,
                    span,
                );
            }

            // Not a reflexive/newtype `from` — fall through to the generic
            // call handling below, which reifies args itself; `arg` here is
            // dropped (no side effects: `reify_expr` is pure TIR shaping).
        }

        // Closure-call shape: bare-ident callee that resolves to a
        // local with `fn(...)` type. Annotate decides this by
        // probing `ctx.lookup`; reify reproduces by checking the
        // ident's local + its resolved type. The same `ctx` reify
        // built during the body walk has every let-bound local in
        // place (per the Gap 7 walk-order invariant), so the lookup
        // returns the same answer.
        if let ast::Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
            && let Some(local) = ctx.lookup(&ident.name)
            && {
                // The callee may be a bare `fn(...)` value or a reference
                // to one (`&fn(...)`, `&mut fn(...)`), possibly behind a
                // fn-type newtype. Mirror `Elaborator::as_fn_signature`:
                // peel references and the ultimate base type before
                // checking for `Function`.
                let table = self.tysys.type_table.borrow();
                let base = table.get_ultimate_base_type(table.peel_refs(local.type_id));
                matches!(table.get(base), crate::tir::ResolvedType::Function { .. })
            }
        {
            let local_index = local.index;
            let local_type_id = local.type_id;
            let callee_expr = TirExpr::new(
                TirExprKind::Local {
                    index: local_index,
                    name: ident.name.clone(),
                },
                local_type_id,
                ident.span,
            );
            // Auto-deref a `&fn` / `&mut fn` callee down to the function
            // value, exactly as `build_indirect_call`'s final
            // `deref_to_value` does in the production path.
            let callee_expr = super::Elaborator::<H>::deref_to_value_static(
                callee_expr,
                ident.span,
                &self.tysys.type_table,
            );
            let arg_exprs: Vec<TirExpr> = call
                .args
                .iter()
                .map(|a| self.reify_expr(a, ctx, None))
                .collect();
            return TirExpr::new(
                TirExprKind::IndirectCall {
                    callee: Box::new(callee_expr),
                    args: arg_exprs,
                },
                recorded_type,
                span,
            );
        }

        // Indirect-call shape: callee is any non-ident expression
        // whose type resolves to a function (e.g. `arr[i](x)`,
        // `(foo.bar)(x)`, `(get_fn())(x)`, `(|x| x)(1)`). Mirrors
        // `Elaborator::resolve_call`'s non-ident-callee path
        // (call.rs:248+).
        if !matches!(&call.callee, ast::Expr::Ident(_)) {
            let callee_expr = self.reify_expr(&call.callee, ctx, None);
            let is_fn = {
                let table = self.tysys.type_table.borrow();
                let base = table.get_ultimate_base_type(table.peel_refs(callee_expr.type_id));
                matches!(table.get(base), crate::tir::ResolvedType::Function { .. })
            };
            if is_fn {
                // Auto-deref a `&fn` / `&mut fn` callee, matching
                // `build_indirect_call`'s `deref_to_value` in production.
                let callee_expr = super::Elaborator::<H>::deref_to_value_static(
                    callee_expr,
                    call.callee.span(),
                    &self.tysys.type_table,
                );
                let arg_exprs: Vec<TirExpr> = call
                    .args
                    .iter()
                    .map(|a| self.reify_expr(a, ctx, None))
                    .collect();
                return TirExpr::new(
                    TirExprKind::IndirectCall {
                        callee: Box::new(callee_expr),
                        args: arg_exprs,
                    },
                    recorded_type,
                    span,
                );
            }
            // Non-fn-typed non-ident callee — annotate already
            // diagnosed it (`TypeError::CalleeNotCallable`).
            // Match the elaborator's recovery shape.
            return TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, span);
        }

        // Free-function call: bare-ident callee that names a current-
        // module or imported function.
        if let ast::Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
        {
            let (callee_module, callee_name) = if self
                .sem
                .decls
                .function_return_types
                .contains_key(&ident.name)
            {
                (self.current_module_source.clone(), ident.name.clone())
            } else if let Some(import_src) = self
                .sem
                .imports
                .imported_type_sources
                .get(&ident.name)
                .cloned()
            {
                let original_name = self
                    .sem
                    .imports
                    .import_original_names
                    .get(&ident.name)
                    .cloned()
                    .unwrap_or_else(|| ident.name.clone());
                (import_src, original_name)
            } else {
                // The callee resolves neither as a local fn-typed
                // value (closure-call branch above) nor as a known
                // free / imported function. The remaining shapes
                // are namespaced calls (`ns::foo(x)`, with `ns`
                // resolved via `sem.imports.namespace_imports`).
                // Annotate has diagnosed truly-unresolved names.
                if let Some(double_colon) = ident.name.find("::") {
                    let ns_prefix = &ident.name[..double_colon];
                    let rest = &ident.name[double_colon + 2..];
                    if let Some(ns_source) =
                        self.sem.imports.namespace_imports.get(ns_prefix).cloned()
                        && !rest.contains("::")
                    {
                        let type_args: Vec<TypeId> = if call.type_args.is_empty() {
                            self.ann_generic_instantiations(call.id)
                                .map(|gi| gi.type_args)
                                .unwrap_or_default()
                        } else {
                            call.type_args
                                .iter()
                                .map(|ty| self.resolve_type(ty))
                                .collect()
                        };
                        let arg_calls: Vec<CallArg> = call
                            .args
                            .iter()
                            .map(|a| CallArg::new(self.reify_expr(a, ctx, None), false))
                            .collect();
                        return TirExpr::new(
                            TirExprKind::Call {
                                func: crate::tir::FunctionRef {
                                    module_source: ns_source,
                                    name: rest.to_string(),
                                    monomorph_info: None,
                                    method_info: None,
                                },
                                type_args,
                                args: arg_calls,
                            },
                            recorded_type,
                            span,
                        );
                    }
                }
                // Unresolved: emit recovery shape matching
                // annotate's diagnostic path.
                return TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, span);
            };

            // Type args: explicit turbofish on the call expression,
            // else the inference recorded by Gap 1.
            let type_args: Vec<TypeId> = if call.type_args.is_empty() {
                self.ann_generic_instantiations(call.id)
                    .map(|gi| gi.type_args)
                    .unwrap_or_default()
            } else {
                call.type_args
                    .iter()
                    .map(|ty| self.resolve_type(ty))
                    .collect()
            };

            // Per-argument expected types come from the recorded resolved
            // param types. They are required for unannotated-param closure
            // args (`|a, b| ...`) coerced to a `fn`-typed (or `fn`-newtype)
            // param, so the closure infers its params and produces the functor
            // specialization the call site needs. Literal re-coercion and
            // `is_mut` per-arg are still handled elsewhere (`coercions`); see
            // `arg_is_unannotated_closure` for why the forward is restricted.
            let call_param_types = self.ann_call_param_types(call.id);
            let args: Vec<CallArg> = call
                .args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let expected = if arg_is_unannotated_closure(a) {
                        call_param_types
                            .as_ref()
                            .and_then(|pts| pts.get(i).copied())
                    } else {
                        None
                    };
                    let arg = self.reify_expr(a, ctx, expected);
                    CallArg::new(arg, false)
                })
                .collect();

            return TirExpr::new(
                TirExprKind::Call {
                    func: crate::tir::FunctionRef {
                        module_source: callee_module,
                        name: callee_name,
                        monomorph_info: None,
                        method_info: None,
                    },
                    type_args,
                    args,
                },
                recorded_type,
                span,
            );
        }

        // Qualified-callee static method `Struct::method(args)`.
        // Reify resolves the prefix to its module via the type
        // lookup (struct / namespace / newtype follow chain) and
        // builds the same mangled `__Struct__method` FunctionRef
        // the elaborator's `resolve_static_method_call_from_qualified`
        // produces. The type-arg list comes from Gap 1's record on
        // the call's AstId (the recording site at call.rs:472
        // already covers impl-args + method-args concatenation).
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some(pos) = ident.name.find("::")
            && !ident.name[pos + 2..].contains("::")
        {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];

            // Flags `Type::none()` / `Type::all()` lower to an
            // `IntLiteral` with the bitmask value (matches
            // `Elaborator::resolve_call`'s flags branch at
            // call.rs:586+).
            let flags = self.type_lookup().flags_case(prefix).cloned();
            if let Some(flags_info) = flags
                && matches!(suffix, "none" | "all")
            {
                let member_count = flags_info.members.len() as u32;
                let value: u64 = match suffix {
                    "none" => 0,
                    "all" => u64::from((1u32 << member_count) - 1),
                    _ => unreachable!(),
                };
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value,
                        repr: value.to_string(),
                    },
                    flags_info.type_id,
                    span,
                );
            }

            // Resolve the prefix struct's module so the
            // FunctionRef points at the impl block's home
            // module. Follows newtype chains the same way
            // `Elaborator::resolve_static_method_call`
            // (method_call.rs:820+) does.
            let lookup = self.type_lookup();
            let struct_module = lookup
                .struct_fields(prefix)
                .map(|info| info.module_source.clone())
                .or_else(|| {
                    lookup
                        .variant_case(prefix)
                        .map(|info| info.module_source.clone())
                })
                .or_else(|| {
                    lookup
                        .resource_type(prefix)
                        .map(|info| info.module_source.clone())
                })
                .unwrap_or_else(|| self.current_module_source.clone());

            let mangled_method_name = crate::name::MethodName::format_local(prefix, None, suffix);

            let type_args: Vec<TypeId> = if call.type_args.is_empty() {
                self.ann_generic_instantiations(call.id)
                    .map(|gi| gi.type_args)
                    .unwrap_or_default()
            } else {
                call.type_args
                    .iter()
                    .map(|ty| self.resolve_type(ty))
                    .collect()
            };

            let arg_calls: Vec<CallArg> = call
                .args
                .iter()
                .map(|a| CallArg::new(self.reify_expr(a, ctx, None), false))
                .collect();

            let method_info =
                crate::name::LocalMethodName::new(prefix.to_string(), None, suffix.to_string());

            return TirExpr::new(
                TirExprKind::Call {
                    func: crate::tir::FunctionRef {
                        module_source: struct_module,
                        name: mangled_method_name,
                        monomorph_info: None,
                        method_info: Some(method_info),
                    },
                    type_args,
                    args: arg_calls,
                },
                recorded_type,
                span,
            );
        }

        // Unrecognised callee shape — annotate diagnosed it.
        TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, span)
    }

    /// Reify the `container[i].method(args)` `IndexMut` rewrite
    /// (Gap 3 desugar tag). Mirrors
    /// `Elaborator::try_resolve_index_mut_method_call`
    /// (`method_lookup.rs:3390`+).
    ///
    /// Two dispatch records drive the shape:
    /// - The inner `IndexMut::index_mut(idx)` call lives on
    ///   `sem.types.operator_dispatch[index_expr.id]` (recorded
    ///   alongside the desugar tag, mirroring Gap 11's Index
    ///   wiring).
    /// - The outer method call's dispatch lives on
    ///   `sem.types.method_dispatch[method_call.id]` (recorded by
    ///   the Stage 4 / Gap 2 path).
    ///
    /// Reify reads both, reifies the container + index, builds
    /// `container.index_mut(idx)`, then adjusts the receiver via
    /// the outer dispatch's `self_kind` / `is_ref_impl` and emits
    /// the outer `MethodCall` TIR.
    fn reify_index_mut_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::CallArg;

        // The AST receiver of the IndexMutMethodCall is always an
        // `Expr::Index` — guaranteed by the elaborator's
        // dispatcher; reify trusts the desugar tag's contract.
        let ast::Expr::Index(index_expr) = &method_call.receiver else {
            panic!(
                "reify_index_mut_method_call: receiver is not an IndexExpr (Gap 3 desugar invariant violated)"
            );
        };

        let inner_dispatch = self
            .ann_operator_dispatch(index_expr.id)
            .expect(
                "reify_index_mut_method_call: inner IndexMut dispatch missing — annotate should have recorded it alongside the IndexMutMethodCall desugar tag",
            );

        let outer_dispatch = self
            .ann_method_dispatch(method_call.id)
            .expect(
                "reify_index_mut_method_call: outer method dispatch missing — annotate should have recorded it via record_method_dispatch",
            );

        // Step 1: build the `container.index_mut(idx)` call.
        let container = self.reify_expr(&index_expr.expr, ctx, None);
        let receiver_for_index_mut = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
            container,
            inner_dispatch.self_kind,
            false,
            index_expr.span,
            &self.tysys.type_table,
        );
        let index_resolved = self.reify_expr(&index_expr.index, ctx, None);
        let index_mut_call = super::Elaborator::<H>::build_tir_method_call(
            receiver_for_index_mut,
            inner_dispatch.function_ref,
            vec![],
            vec![CallArg::new(index_resolved, false)],
            inner_dispatch.return_type,
            index_expr.span,
        );

        // Step 2: adjust the index_mut result for the outer method's
        // self_kind and build the outer MethodCall TIR.
        let receiver_for_method = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
            index_mut_call,
            outer_dispatch.self_kind,
            outer_dispatch.is_ref_impl,
            method_call.span,
            &self.tysys.type_table,
        );

        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();
        let args: Vec<CallArg> = method_call
            .args
            .iter()
            .map(|a| CallArg::new(self.reify_expr(a, ctx, None), false))
            .collect();

        let result_type = if outer_dispatch.return_type == crate::tir::TypeTable::UNKNOWN {
            recorded_type
        } else {
            outer_dispatch.return_type
        };
        super::Elaborator::<H>::build_tir_method_call(
            receiver_for_method,
            outer_dispatch.function_ref,
            type_args,
            args,
            result_type,
            method_call.span,
        )
    }

    /// Reify a `MethodCallExpr`. This is the cleanest Stage 5 path:
    /// every decision — the resolved `FunctionRef`, the receiver-
    /// adjustment kind, the ref-impl flag, the expression's final type
    /// — is already on `sem.types` (`method_dispatch` from Stage 4,
    /// `expression_types` from Stage 4, `is_ref_impl` from Stage 5
    /// Gap 2). Reify reads all four and emits the same TIR shape
    /// `Elaborator::resolve_method_call_with` produced, sharing the
    /// receiver-adjustment helper
    /// [`super::Elaborator::adjust_receiver_for_self_kind_static`].
    ///
    /// The `IndexMutMethodCall` desugar (Gap 3) routes through here
    /// too: when `sem.types.desugars[id] == IndexMutMethodCall`, the
    /// receiver is an `IndexExpr` and reify must materialise the
    /// `__index_mut_val` local before dispatching the method. That
    /// branch is documented inline and currently `todo!`'d.
    fn reify_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};

        // IndexMut rewrite gets first crack — when the elaborator
        // tagged this call as `IndexMutMethodCall`, the receiver is an
        // index expression that needs `__index_mut_val` synthesis.
        if matches!(
            self.ann_desugars(method_call.id),
            Some(super::sem::types::DesugarKind::IndexMutMethodCall)
        ) {
            return self.reify_index_mut_method_call(method_call, ctx, recorded_type);
        }

        // Synthetic-call shortcuts: `tuple.len()` / `tuple.zip()`
        // bypass method dispatch entirely (the elaborator's
        // `resolve_method_call_with` short-circuits at the
        // receiver-type check, leaving no `method_dispatch` entry).
        // Reify recognises tuple-typed receivers and emits the
        // direct TIR shape. See WEP §"Synthetic call sites stay
        // annotation-free by design".
        if matches!(method_call.method.as_str(), "len" | "zip") {
            let receiver = self.reify_expr(&method_call.receiver, ctx, None);
            let base_type = self.tysys.type_table.borrow().get(receiver.type_id).clone();
            let is_tuple_receiver = matches!(
                base_type,
                crate::tir::ResolvedType::GenericInstance { ref name, ref module_source, .. }
                    if crate::tir::TypeTable::is_tuple_type(name, module_source),
            );
            if is_tuple_receiver {
                return match method_call.method.as_str() {
                    "len" => {
                        let len = self
                            .tysys
                            .type_table
                            .borrow()
                            .as_tuple(receiver.type_id)
                            .map(|elems| elems.len())
                            .unwrap_or(0) as i64;
                        TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: len as u64,
                                repr: len.to_string(),
                            },
                            TypeTable::I32,
                            method_call.span,
                        )
                    }
                    "zip" => {
                        // Mirror the elaborator (`method_call.rs`): a
                        // concrete tuple-of-tuples transposes inline now;
                        // only a type-pack receiver defers expansion to the
                        // monomorphiser via `TupleZip`. Non-generic bodies
                        // never reach the monomorphiser, so emitting
                        // `TupleZip` here would hit `lower::translate`'s
                        // `unreachable!`.
                        let base_type_id = receiver.type_id;
                        if self.type_contains_pack(base_type_id) {
                            TirExpr::new(
                                TirExprKind::TupleZip {
                                    expr: Box::new(receiver),
                                },
                                recorded_type,
                                method_call.span,
                            )
                        } else {
                            // [[A0, A1], [B0, B1]].zip() → [[A0, B0], [A1, B1]]
                            let outer_elems = self
                                .tysys
                                .type_table
                                .borrow()
                                .as_tuple(base_type_id)
                                .unwrap();
                            let inner_arities: Vec<Vec<TypeId>> = outer_elems
                                .iter()
                                .map(|e| self.tysys.type_table.borrow().as_tuple(*e).unwrap())
                                .collect();
                            let arity = inner_arities[0].len();
                            let num_rows = outer_elems.len();
                            let mut col_exprs = Vec::with_capacity(arity);
                            for col in 0..arity {
                                let mut row_exprs = Vec::with_capacity(num_rows);
                                for (row, row_types) in inner_arities.iter().enumerate() {
                                    let row_access = TirExpr::new(
                                        TirExprKind::FieldAccess {
                                            expr: Box::new(receiver.clone()),
                                            field_index: row as u32,
                                            field_name: row.to_string(),
                                        },
                                        outer_elems[row],
                                        method_call.span,
                                    );
                                    let cell = TirExpr::new(
                                        TirExprKind::FieldAccess {
                                            expr: Box::new(row_access),
                                            field_index: col as u32,
                                            field_name: col.to_string(),
                                        },
                                        row_types[col],
                                        method_call.span,
                                    );
                                    row_exprs.push(cell);
                                }
                                let col_types: Vec<TypeId> =
                                    inner_arities.iter().map(|row| row[col]).collect();
                                let col_tuple_type =
                                    self.tysys.type_table.borrow_mut().make_tuple(col_types);
                                col_exprs.push(TirExpr::new(
                                    TirExprKind::TupleLiteral {
                                        elements: row_exprs,
                                    },
                                    col_tuple_type,
                                    method_call.span,
                                ));
                            }
                            TirExpr::new(
                                TirExprKind::TupleLiteral {
                                    elements: col_exprs,
                                },
                                recorded_type,
                                method_call.span,
                            )
                        }
                    }
                    _ => unreachable!(),
                };
            }
        }

        // Dispatch decision (Stage 4 + Gap 2 record).
        let dispatch = self.ann_method_dispatch(method_call.id).unwrap_or_else(|| {
            // Method lookup failed during annotate (error-recovery
            // path). Reify produces a placeholder `Unit` of `ERROR`
            // type so downstream phases see the same shape annotate
            // would have built; the actual diagnostic was already
            // emitted by the elaborator.
            panic!(
                "reify_method_call: dispatch annotation missing for `{}` — \
                     annotate should have recorded or short-circuited via desugar",
                method_call.method
            )
        });

        // Reify receiver and adjust per the dispatch contract. Stage 5
        // shares the adjuster with the elaborator so the same TIR shape
        // (Unary{Ref}/Unary{MutRef}/Deref wrapping) lands.
        let raw_receiver = self.reify_expr(&method_call.receiver, ctx, None);

        // Track implicit `&mut self` borrowing for primitive local receivers,
        // mirroring `Elaborator::resolve_method_call_with` (method_call.rs:517):
        // a primitive is value-copied by default, so `x.bump()` must mark `x`
        // address-taken or the boxing pass won't write the mutation back.
        let needs_implicit_mut_borrow =
            !dispatch.is_ref_impl && matches!(dispatch.self_kind, ast::SelfKind::MutRef) && {
                let tt = self.tysys.type_table.borrow();
                !matches!(
                    tt.get(raw_receiver.type_id),
                    crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
                ) && matches!(
                    tt.get(tt.get_ultimate_base_type(raw_receiver.type_id)),
                    crate::tir::ResolvedType::Primitive(_)
                )
            };
        if needs_implicit_mut_borrow && let TirExprKind::Local { index, .. } = &raw_receiver.kind {
            ctx.address_taken_locals.insert(*index);
        }

        let adjusted_receiver = super::Elaborator::<H>::adjust_receiver_for_self_kind_static(
            raw_receiver,
            dispatch.self_kind,
            dispatch.is_ref_impl,
            method_call.span,
            &self.tysys.type_table,
        );

        // Method-level type args for the TIR `MethodCall` node. The
        // monomorphizer's `collect_func_instantiation_sites` keys off this
        // field to queue `Struct^Trait::method<Args>` instances, so it
        // must carry the *resolved* args — including ones inferred from
        // argument types when there is no turbofish (`c.transform(42)`
        // infers `T = i32`). Explicit turbofish resolves against the
        // current type-param scope; otherwise fall back to the inferred
        // args the elaborator baked into the recorded `FunctionRef`'s
        // `monomorph_info` (production passes the same vector as the node
        // type args at `method_call.rs:817`).
        let type_args: Vec<TypeId> = if method_call.type_args.is_empty() {
            dispatch
                .function_ref
                .monomorph_info
                .as_ref()
                .map(|mi| mi.method_type_args.clone())
                .unwrap_or_default()
        } else {
            method_call
                .type_args
                .iter()
                .map(|ty| self.resolve_type(ty))
                .collect()
        };

        // Per-arg `is_mut` comes from the recorded `MethodDispatch`
        // (drained from `lookup_method_param_is_mut` at annotate time).
        // Zip with the AST args so call sites with fewer args than
        // declared (a Stage-5 recovery shape) still produce the
        // right is_mut for the args we have.
        let mut args: Vec<crate::tir::CallArg> = method_call
            .args
            .iter()
            .zip(
                dispatch
                    .param_is_mut
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(false)),
            )
            .map(|(a, is_mut)| {
                let arg_tir = self.reify_expr(a, ctx, None);
                crate::tir::CallArg::new(arg_tir, is_mut)
            })
            .collect();

        // Pad missing trailing args with the method's defaults.
        // Mirrors method_call.rs:481+; the recorded `param_names` /
        // `param_defaults` arrive on `MethodDispatch` from annotate.
        if args.len() < dispatch.param_defaults.len() {
            let mut subs: IndexMap<String, ast::Expr> = IndexMap::default();
            for (i, arg_ast) in method_call.args.iter().enumerate() {
                if let Some(name) = dispatch.param_names.get(i) {
                    subs.insert(name.clone(), arg_ast.clone());
                }
            }
            for i in args.len()..dispatch.param_defaults.len() {
                let Some(Some(default_ast)) = dispatch.param_defaults.get(i) else {
                    break;
                };
                let mut default_expr = default_ast.clone();
                default_expr.substitute_idents(&subs);
                let resolved = self.reify_expr(&default_expr, ctx, None);
                let is_mut = dispatch.param_is_mut.get(i).copied().unwrap_or(false);
                args.push(crate::tir::CallArg::new(resolved, is_mut));
                if let Some(name) = dispatch.param_names.get(i) {
                    subs.insert(name.clone(), default_expr);
                }
            }
        }

        // The call's result type is the resolved method's return type
        // (recorded on the dispatch), not the per-`AstId` `expression_types`
        // entry: that entry can carry a wrong type for the call site, which
        // would make a unit-returning call look value-producing and emit a
        // spurious `drop` of a value-less call (Wasm stack underflow). Fall
        // back to `recorded_type` only if the dispatch somehow lacks it.
        let result_type = if dispatch.return_type == TypeTable::UNKNOWN {
            recorded_type
        } else {
            dispatch.return_type
        };
        super::Elaborator::<H>::build_tir_method_call(
            adjusted_receiver,
            dispatch.function_ref,
            type_args,
            args,
            result_type,
            method_call.span,
        )
    }

    /// Resolve a struct field name to its `(index, name)` pair via
    /// the resolved struct type. Tuple-struct projections (`t.0`)
    /// resolve through the tuple element index. Returns `(0, name)` on
    /// lookup failure so reify doesn't panic on a type the dispatch
    /// hasn't ported yet — the produced TIR is wrong, but downstream
    /// validation flags it loudly.
    /// Resolve a field access to its `(index, canonical_name,
    /// field_type)` against the receiver's struct decl. The field type is
    /// generic-substituted with the receiver's `type_args` and is the
    /// authoritative source for the access's `TirExpr::type_id` — unlike
    /// `expression_types[field.id]`, which collides across template
    /// sub-parsers (WEP 2026-05-26 gotcha #1). `None` for the type means
    /// the receiver was not a known struct; the caller falls back to the
    /// recorded type.
    fn lookup_struct_field_index(
        &self,
        receiver_type: TypeId,
        field_name: &str,
    ) -> (u32, String, Option<TypeId>) {
        use crate::tir::ResolvedType;
        let resolved = self.tysys.type_table.borrow().get(receiver_type).clone();
        // Keep the receiver's `module_source` so same-named structs in
        // different modules (a local `Pair` vs an imported `helper::Pair`)
        // resolve their fields against the right decl. A name-only lookup
        // finds whichever the current module sees first, mapping
        // `remote.y` onto the wrong field index.
        let (struct_name, module_source, type_args): (String, Option<ModuleSource>, Vec<TypeId>) =
            match resolved {
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                } => (name, Some(module_source), vec![]),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } => {
                    // Tuple projection (`t.0`): a tuple is a `GenericInstance`
                    // named "Tuple" with no struct decl, so the struct-fields
                    // lookup below would miss and fall to the `(0, …)`
                    // fallback — collapsing every `t.N` onto field 0, which
                    // SROA then keys on. Resolve the numeric field name into
                    // the element index directly, mirroring the elaborator's
                    // `lookup_field_type` tuple branch (expr.rs:1513).
                    if crate::tir::TypeTable::is_tuple_type(&name, &module_source)
                        && let Ok(index) = field_name.parse::<usize>()
                        && index < type_args.len()
                    {
                        return (index as u32, field_name.to_string(), Some(type_args[index]));
                    }
                    (name, Some(module_source), type_args)
                }
                // Peel references and newtypes and recurse, mirroring the
                // elaborator's `lookup_field_type` (expr.rs:1500): `&Point`,
                // `&mut Point`, a newtype `Location = Point`, and chained
                // newtypes / `&Location` all resolve their fields against the
                // ultimate underlying struct. Without the `Newtype` arm a
                // `loc: Location` receiver fell to the fallback and every field
                // reified with `field_index = 0`, which `nir/sroa` later keys
                // on to alias `.y` onto the `.x` scalar local.
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    return self.lookup_struct_field_index(inner, field_name);
                }
                ResolvedType::Newtype { base_type, .. } => {
                    return self.lookup_struct_field_index(base_type, field_name);
                }
                _ => return (0, field_name.to_string(), None),
            };

        let resolve_in = |info: &super::types::StructFieldInfo| {
            info.fields
                .iter()
                .enumerate()
                .find(|(_, (n, _, _))| n == field_name)
                .map(|(idx, (n, ty, _))| (idx as u32, n.clone(), *ty))
        };
        let found = if let Some(info) = module_source
            .as_ref()
            .and_then(|ms| self.tysys.all_struct_fields.get(ms))
            .and_then(|m| m.get(&struct_name))
        {
            resolve_in(info)
        } else {
            self.type_lookup()
                .struct_fields(&struct_name)
                .and_then(resolve_in)
        };
        let Some((idx, canonical, raw_field_type)) = found else {
            return (0, field_name.to_string(), None);
        };

        let field_type = if type_args.is_empty() {
            raw_field_type
        } else {
            let substitution: crate::hashmap::IndexMap<u32, TypeId> = (0..type_args.len() as u32)
                .zip(type_args.iter().copied())
                .collect();
            self.tysys
                .type_table
                .borrow_mut()
                .substitute_type_params(raw_field_type, &substitution)
        };
        (idx, canonical, Some(field_type))
    }

    /// Run `body` with reify's module perspective (`current_module_source`
    /// / `current_module_items` / `sem`) swapped to `module` when it is a
    /// different, loaded module, restoring the originals afterward. Used to
    /// reify an AST fragment that belongs to another module (e.g. an
    /// associated constant's body): the `ann_*` accessors key on
    /// `current_module_source`, so swapping it makes their `SymbolKey`
    /// lookups hit that module's `ModuleSemantics` rather than the use
    /// site's. Same mechanism the default-argument path uses inline
    /// (cross-module AST reify). A no-op when `module` is the current module
    /// or is not loaded.
    fn with_const_module_perspective<R>(
        &mut self,
        module: &ModuleSource,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let swap = if module == &self.current_module_source {
            None
        } else {
            match (
                self.loaded_modules.get(module),
                self.all_module_semantics.get(module),
            ) {
                (Some(m), Some(sem)) => Some((m.items.as_slice(), sem)),
                _ => None,
            }
        };
        let saved = swap.map(|(items, sem)| {
            (
                std::mem::replace(&mut self.current_module_source, module.clone()),
                std::mem::replace(&mut self.current_module_items, items),
                std::mem::replace(&mut self.sem, sem),
            )
        });
        let result = body(self);
        if let Some((src, items, sem)) = saved {
            self.current_module_source = src;
            self.current_module_items = items;
            self.sem = sem;
        }
        result
    }

    /// Reify a bare identifier reference. Local lookup goes through
    /// the per-function context (`FunctionContext::lookup`, walk-order
    /// invariant — Gap 7). Non-local idents (globals, function refs,
    /// enum / variant ctors) read [`super::sem::ModuleDecls`] to pick
    /// the right TIR shape; full coverage of every kind is staged.
    fn reify_ident(
        &mut self,
        ident: &ast::IdentExpr,
        recorded_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::TirExprKind;

        // 1. Local / capture lookup, mirroring `resolve_ident`
        //    (expr.rs:534+). Use the local's stored type instead of
        //    `recorded_type`: template-string sub-parsers restart
        //    `next_ast_id` at 0 (parser.rs:5175), so multiple
        //    interpolations collide on `AstId(0)` and the last write
        //    to `expression_types` wins.
        if let Some(var_ref) = ctx.lookup_or_capture(&ident.name) {
            match var_ref {
                super::types::VarRef::Local { index, type_id, .. } => {
                    return TirExpr::new(
                        TirExprKind::Local {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                super::types::VarRef::Capture { index, type_id, .. } => {
                    return TirExpr::new(
                        TirExprKind::Capture {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                super::types::VarRef::DerefCapture {
                    index,
                    ref_type_id,
                    inner_type_id,
                    ..
                } => {
                    let capture_expr = TirExpr::new(
                        TirExprKind::Capture {
                            index,
                            name: format!("__deref_cap_{index}"),
                        },
                        ref_type_id,
                        ident.span,
                    );
                    return TirExpr::new(
                        TirExprKind::Unary {
                            op: crate::tir::TirUnaryOp::Deref,
                            expr: Box::new(capture_expr),
                        },
                        inner_type_id,
                        ident.span,
                    );
                }
            }
        }

        // 2. Current-module global.
        if self
            .sem
            .decls
            .current_module_globals
            .contains_key(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                recorded_type,
                ident.span,
            );
        }

        // 3. Imported global.
        if let Some((src, original_name, _ty, _is_mut)) =
            self.sem.decls.imported_globals.get(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: src.clone(),
                    name: original_name.clone(),
                },
                recorded_type,
                ident.span,
            );
        }

        // 3b. Current-module global declared in the module AST but absent
        //     from `current_module_globals`. This happens when reify is
        //     walking a *swapped-in* callee module (a default-argument
        //     expression resolved in the callee's scope) whose
        //     `ModuleSemantics` came from the stdlib snapshot, which does
        //     not rehydrate `current_module_globals`. The module's AST
        //     items are available (the swap sets `current_module_items`),
        //     so resolve the global from there — mirroring production's
        //     `resolve_ident_in_fallback_module` (expr.rs:936). Without this
        //     a callee-module global default (e.g. `Z_DEFAULT_COMPRESSION`)
        //     resolves to `()` and the call lowers to an invalid module.
        if let Some(global_decl) = self
            .current_module_items
            .iter()
            .find_map(|item| match item {
                ast::Item::Global(g) if g.name == ident.name => Some(g),
                _ => None,
            })
        {
            // 7-A: the global's declared type was resolved by `annotate_decls`
            // and lives on `current_module_globals`; read it back (same source
            // as `reify_global`), re-resolving only if unrecorded.
            let ty = self
                .sem
                .decls
                .current_module_globals
                .get(&ident.name)
                .map(|(t, _)| *t)
                .unwrap_or_else(|| self.resolve_type(&global_decl.ty));
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                ty,
                ident.span,
            );
        }

        // 4. Associated constant (e.g. `f64::PI`, `i32::MAX`). The
        //    elaborator inlines these to the resolved expression at
        //    every use site; reify reproduces the same inlining by
        //    re-reifying the constant's `Expr` from
        //    `sem.decls.associated_constants`. The constant's body is
        //    independent of the call site's scope (a pure literal /
        //    static expression in practice), so reify uses the
        //    surrounding `ctx` directly — matches the elaborator's
        //    `resolve_expr(&const_expr, ctx, …)` (expr.rs:594–605).
        if let Some((const_module, const_ty, const_expr)) = self
            .sem
            .decls
            .associated_constants
            .get(&ident.name)
            .cloned()
        {
            let type_id = self.resolve_type(&const_ty);
            // The constant's body lives in its *defining* module (e.g.
            // `pub const MAX: i32 = 2147483647;` in primitive.wado). Its
            // `AstId`s index that module's `ModuleSemantics`, not the use
            // site's, and `AstId`s are only unique within a module — so
            // reifying the body under `self.sem` (the current module) can
            // pick up a colliding `AstId`'s recorded type and mis-type the
            // literal (e.g. `i32::MAX`'s `2147483647` as an f64). Reify the
            // body under the defining module's perspective so every
            // annotation lookup hits the right module's records.
            let resolved = self.with_const_module_perspective(&const_module, |this| {
                this.reify_expr(&const_expr, ctx, Some(type_id))
            });
            return TirExpr::new(resolved.kind, type_id, ident.span);
        }

        // 4b. Primitive associated constant (`i32::MAX`, `u8::MIN`, …) that
        //     is not in `associated_constants`. This happens when reify is
        //     walking a swapped-in callee module (a default-argument
        //     expression — e.g. `max_output: i32 = i32::MAX`) whose
        //     `ModuleSemantics` came from the stdlib snapshot, which does
        //     not rehydrate `associated_constants`. The value is a compile
        //     -time constant of the named primitive type, so emit it as a
        //     typed integer literal directly.
        if let Some((prefix, suffix)) = ident.name.split_once("::")
            && !suffix.contains("::")
            && let Some((value, prim_type)) = primitive_int_assoc_const(prefix, suffix)
        {
            return TirExpr::new(
                TirExprKind::IntLiteral {
                    value: value as u64,
                    repr: value.to_string(),
                },
                prim_type,
                ident.span,
            );
        }

        // 5. Free function reference — the ident names a function in
        //    the current module or imported via a `use` declaration.
        //    Emit `TirExprKind::FuncRef` with the recorded
        //    instantiation's type_args when present.
        if self
            .sem
            .decls
            .function_return_types
            .contains_key(&ident.name)
        {
            let type_args = self
                .ann_generic_instantiations(ident.id)
                .map(|gi| gi.type_args)
                .unwrap_or_default();
            return TirExpr::new(
                TirExprKind::FuncRef {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                    type_args,
                },
                recorded_type,
                ident.span,
            );
        }
        if let Some(import_src) = self
            .sem
            .imports
            .imported_type_sources
            .get(&ident.name)
            .cloned()
        {
            let original_name = self
                .sem
                .imports
                .import_original_names
                .get(&ident.name)
                .cloned()
                .unwrap_or_else(|| ident.name.clone());
            // Imports through `use` collapse types + functions into the
            // same `imported_type_sources` map; the type / variant /
            // enum / flags / resource cases were already handled
            // above and would have returned. Anything left here is a
            // function import.
            let type_args = self
                .ann_generic_instantiations(ident.id)
                .map(|gi| gi.type_args)
                .unwrap_or_default();
            return TirExpr::new(
                TirExprKind::FuncRef {
                    module_source: import_src,
                    name: original_name,
                    type_args,
                },
                recorded_type,
                ident.span,
            );
        }

        // 6. Qualified case path `Type::Case`. Variant / enum / flags
        //    are checked in the same priority order as
        //    `Elaborator::resolve_ident` (expr.rs:607+). The
        //    namespace-import form `ns::Type::Case` (two `::`
        //    separators) is handled by a dedicated branch in the
        //    elaborator that resolves the namespace alias first;
        //    that path stays a `todo!` until the dispatcher gets it.
        if let Some(pos) = ident.name.find("::") {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];

            // Two-segment qualified path is "Type::Case". Anything with
            // a further `::` is `ns::Type::Case` (namespace path) —
            // defer to a later branch.
            if !suffix.contains("::") {
                let lookup = self.type_lookup();

                // Variant case.
                if let Some(variant_info) = lookup.variant_case(prefix).cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == suffix)
                        .map(|(i, c)| (i, c.clone()))
                {
                    // Generic variants record the instance type +
                    // type_args via Gap 1 — read them. Non-generic
                    // variants leave no record and the bare
                    // `recorded_type` already names the right
                    // `Variant` TypeId.
                    let variant_type = self
                        .ann_generic_instantiations(ident.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload: None,
                        },
                        variant_type,
                        ident.span,
                    );
                }

                // Enum case.
                if let Some(enum_info) = lookup.enum_case(prefix).cloned()
                    && let Some(case_data) = enum_info.find_case(suffix).cloned()
                {
                    let enum_type = self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .make_enum(enum_info.name.clone(), enum_info.module_source);
                    return TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type,
                            case_index: case_data.index,
                            case_name: case_data.name,
                        },
                        enum_type,
                        ident.span,
                    );
                }

                // Flags member.
                if let Some(flags_info) = lookup.flags_case(prefix).cloned()
                    && let Some(member) = flags_info
                        .members
                        .iter()
                        .find(|m| m.name == suffix)
                        .cloned()
                {
                    return TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: u64::from(member.bitmask),
                            repr: member.bitmask.to_string(),
                        },
                        flags_info.type_id,
                        ident.span,
                    );
                }
            }
        }

        // 7. Namespace-imported path `ns::Type::Case` (and variants
        //    with type args). The `ns::` prefix maps via
        //    `sem.imports.namespace_imports` to the namespace's
        //    source module; reify then resolves against the
        //    namespace's `tysys.all_*` tables (rather than the
        //    current module's).
        if let Some(double_colon) = ident.name.find("::") {
            let ns_prefix = &ident.name[..double_colon];
            let rest = &ident.name[double_colon + 2..];
            if let Some(ns_source) = self.sem.imports.namespace_imports.get(ns_prefix).cloned()
                && let Some(inner_double_colon) = rest.find("::")
            {
                let type_name = &rest[..inner_double_colon];
                let case_name = &rest[inner_double_colon + 2..];

                // Variant case in the namespace's module.
                if let Some(variant_info) = self
                    .tysys
                    .all_variant_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == case_name)
                        .map(|(i, c)| (i, c.clone()))
                {
                    let variant_type = self
                        .ann_generic_instantiations(ident.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or_else(|| {
                            self.tysys.type_table.borrow_mut().make_variant(
                                variant_info.name.clone(),
                                variant_info.module_source.clone(),
                            )
                        });
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload: None,
                        },
                        variant_type,
                        ident.span,
                    );
                }

                // Enum case in the namespace's module.
                if let Some(enum_info) = self
                    .tysys
                    .all_enum_cases
                    .get(&ns_source)
                    .and_then(|m| m.get(type_name))
                    .cloned()
                    && let Some(case_data) = enum_info.find_case(case_name).cloned()
                {
                    let enum_type = self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .make_enum(enum_info.name.clone(), enum_info.module_source);
                    return TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type,
                            case_index: case_data.index,
                            case_name: case_data.name,
                        },
                        enum_type,
                        ident.span,
                    );
                }
            }
        }

        // No remaining recognised ident kind — the elaborator would
        // have diagnosed an unknown identifier at annotate time.
        // Match the elaborator's recovery shape so reify doesn't
        // panic on a known-bad input.
        let _ = recorded_type;
        TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, ident.span)
    }

    /// Reify a literal expression into its TIR shape. The recorded
    /// `TypeId` from `sem.types.expression_types` carries the final
    /// numeric type (e.g. an `i32` literal coerced to `i64` is recorded
    /// as `i64`), so this helper does not re-run literal-type defaulting.
    /// Replay an `i128` / `u128` numeric-literal coercion recorded by
    /// annotate (`coercion::try_coerce_numeric_literal`). Returns `None`
    /// for every other shape so the caller falls through to the normal
    /// walk. The 128-bit types are prelude structs, so the coerced value
    /// is materialized by a `from_u64` / `from_i64` / `from_pair` call
    /// rather than a bare literal; all other `NumericLiteral` coercions
    /// are free (the literal already carries the coerced type).
    fn try_reify_int128_coercion(&self, expr: &ast::Expr) -> Option<TirExpr> {
        let choice = self.ann_coercions(expr.id())?;
        if choice.kind != super::sem::types::CoercionKind::NumericLiteral {
            return None;
        }
        let target_type = choice.target_type;
        let name = match self.tysys.type_table.borrow().get(target_type).clone() {
            crate::tir::ResolvedType::Struct { name, .. } if name == "u128" || name == "i128" => {
                name
            }
            _ => return None,
        };

        // Plain literal, or the negated `-NUM` shape whose coercion is
        // keyed on the enclosing `Unary` node.
        let (repr, negated) = match expr {
            ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Number(repr),
                ..
            }) => (repr.clone(), false),
            ast::Expr::Unary(unary) if unary.op == ast::UnaryOp::Neg => match &unary.expr {
                ast::Expr::Literal(ast::LiteralExpr {
                    value: ast::Literal::Number(repr),
                    ..
                }) => (repr.clone(), true),
                _ => return None,
            },
            _ => return None,
        };

        let parse_result = if name == "u128" {
            super::util::parse_u128_literal(&repr).map(|v| v as i128)
        } else if negated {
            super::util::parse_i128_literal(&format!("-{repr}"))
        } else {
            super::util::parse_i128_literal(&repr)
        };
        let value = parse_result.ok()?;

        Some(super::coercion::build_int128_literal_call(
            &name,
            value,
            &repr,
            !negated,
            target_type,
            expr.span(),
        ))
    }

    /// Replay an `expr as i128/u128` cast. Literal and negated-literal
    /// operands construct the value directly; a general numeric operand
    /// becomes `name::from_u64/from_i64(operand as u64/i64)`. Returns
    /// `None` for non-128-bit targets (normal cast) and for non-numeric
    /// operands (the caller's bare-cast fallback handles those). Mirrors
    /// `Elaborator::resolve_cast`.
    fn try_reify_int128_cast(
        &mut self,
        cast: &ast::CastExpr,
        target_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> Option<TirExpr> {
        let name = match self.tysys.type_table.borrow().get(target_type).clone() {
            crate::tir::ResolvedType::Struct { name, .. } if name == "u128" || name == "i128" => {
                name
            }
            _ => return None,
        };

        // Literal operand: `1042 as u128`.
        if let ast::Expr::Literal(ast::LiteralExpr {
            value: ast::Literal::Number(repr),
            ..
        }) = &cast.expr
            && !super::util::is_float_only_literal(repr)
        {
            let parsed = if name == "u128" {
                super::util::parse_u128_literal(repr).map(|v| v as i128)
            } else {
                super::util::parse_i128_literal(repr)
            };
            if let Ok(value) = parsed {
                return Some(super::coercion::build_int128_literal_call(
                    &name,
                    value,
                    repr,
                    true,
                    target_type,
                    cast.span,
                ));
            }
        }

        // Negated literal operand (i128 only): `-170... as i128`.
        if name == "i128"
            && let ast::Expr::Unary(unary) = &cast.expr
            && unary.op == ast::UnaryOp::Neg
            && let ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Number(repr),
                ..
            }) = &unary.expr
            && !super::util::is_float_only_literal(repr)
            && let Ok(value) = super::util::parse_i128_literal(&format!("-{repr}"))
        {
            return Some(super::coercion::build_int128_literal_call(
                &name,
                value,
                repr,
                false,
                target_type,
                unary.span,
            ));
        }

        // General numeric operand: `x as u128` →
        // `u128::from_u64(x as u64)`. `inner` is reified once here; a
        // non-numeric operand (no valid construction) emits the bare cast
        // directly rather than re-reifying through the caller's fallback.
        let inner = self.reify_expr(&cast.expr, ctx, None);
        let source_is_numeric = {
            let tt = self.tysys.type_table.borrow();
            tt.is_integer(inner.type_id) || tt.is_float(inner.type_id)
        };
        if !source_is_numeric {
            return Some(TirExpr::new(
                crate::tir::TirExprKind::Cast {
                    expr: Box::new(inner),
                    target_type,
                },
                target_type,
                cast.span,
            ));
        }
        let intermediate_type = if name == "u128" {
            crate::tir::TypeTable::U64
        } else {
            crate::tir::TypeTable::I64
        };
        let casted = TirExpr::new(
            crate::tir::TirExprKind::Cast {
                expr: Box::new(inner),
                target_type: intermediate_type,
            },
            intermediate_type,
            cast.span,
        );
        Some(super::coercion::build_int128_from_intermediate(
            &name,
            casted,
            target_type,
            cast.span,
        ))
    }

    fn reify_literal(
        &mut self,
        lit: &ast::LiteralExpr,
        recorded_type: TypeId,
        ctx: &FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};
        let kind = match &lit.value {
            ast::Literal::Number(repr) => {
                // Parse the literal value with the shared `util` helpers
                // (which handle digit separators, scientific notation, and
                // hex/oct/bin radix) rather than a hand-rolled decoder.
                // The *recorded type* decides whether to emit an Int or a
                // Float TIR literal; the literal's *syntactic form*
                // (`is_float_only_literal`) decides how to read its value.
                // A radix/scientific int literal coerced to a float target
                // (`let x: f64 = 0xFF` / `1e2`) reads as an integer then
                // converts; a float literal to an int target never occurs
                // (the elaborator rejects it). Mirrors
                // `Elaborator::resolve_numeric_literal` (expr.rs:337) plus
                // the numeric coercion.
                // Peel newtypes (`type Meters = f64`) to the ultimate base
                // so a float literal bound to a float-newtype target still
                // takes the float path; otherwise it falls to the integer
                // path and codegen sees `i32` where `f64` is expected.
                let base_target = self
                    .tysys
                    .type_table
                    .borrow()
                    .get_ultimate_base_type(recorded_type);
                // A float-only literal (`1.0`, `0.0`, `1e2`) is a float
                // regardless of the recorded type: when the recorded type is
                // missing/UNKNOWN (e.g. a stdlib const body whose
                // `expression_types` entry is absent from the cached
                // snapshot) the syntactic form is authoritative, matching
                // production's `resolve_numeric_literal` (expr.rs:337). An
                // integer literal still defers to the recorded type so
                // `let x: f64 = 1` takes the float path via `is_float_target`.
                let is_float_target = base_target == TypeTable::F32
                    || base_target == TypeTable::F64
                    || (recorded_type == TypeTable::UNKNOWN
                        && super::util::is_float_only_literal(repr));
                if is_float_target {
                    let value: f64 = if super::util::is_float_only_literal(repr) {
                        super::util::parse_float_literal(repr).unwrap_or(0.0)
                    } else {
                        super::util::parse_u128_literal(repr)
                            .map(|v| v as f64)
                            .unwrap_or(0.0)
                    };
                    // The literal's *type* must be a concrete float, not the
                    // (possibly UNKNOWN) recorded type: a float-only literal
                    // with no recorded type defaults to `f64` (matching
                    // production's `resolve_numeric_literal`). Leaving it
                    // UNKNOWN makes lowering pick an integer op for the
                    // surrounding arithmetic (`f64.div` -> `i32.div_s`).
                    // f32 target keeps f32; otherwise (f64 target, or no
                    // recorded type) an untyped float literal defaults to f64.
                    let float_type = if base_target == TypeTable::F32 {
                        TypeTable::F32
                    } else {
                        TypeTable::F64
                    };
                    return TirExpr::new(
                        TirExprKind::FloatLiteral {
                            value,
                            repr: repr.clone(),
                        },
                        float_type,
                        lit.span,
                    );
                } else {
                    let value = super::util::parse_u128_literal(repr).unwrap_or(0) as u64;
                    TirExprKind::IntLiteral {
                        value,
                        repr: repr.clone(),
                    }
                }
            }
            ast::Literal::String(s) => {
                // Decode escape sequences (`\"`, `\n`, `\\`, …) the same
                // way the elaborator does (expr.rs:403) — the AST holds
                // the raw source text. Without this a literal like
                // `"{\""` reaches codegen with the backslash intact and
                // serializes as `{\"` instead of `{"`.
                let value = super::util::unescape_string(s).unwrap_or_default();
                TirExprKind::StringLiteral(value)
            }
            ast::Literal::Char(s) => {
                // The Char literal is the raw source text (e.g. `'a'`,
                // `'\n'`). Decode escapes via the shared `unescape_char`,
                // matching the elaborator — a hand-rolled
                // `chars().next()` reads the backslash of `'\n'` as `'\'`,
                // which then fails to match a `'\n'` pattern that decodes
                // correctly.
                let ch = super::util::unescape_char(s).unwrap_or('\0');
                TirExprKind::CharLiteral(ch)
            }
            ast::Literal::Bool(b) => TirExprKind::BoolLiteral(*b),
            ast::Literal::Null => TirExprKind::Null,
            ast::Literal::Unit => TirExprKind::Unit,
            ast::Literal::LocationFunction => TirExprKind::StringLiteral(ctx.function_name.clone()),
            ast::Literal::LocationFile => {
                // `#file` — current module source as a string.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                return TirExpr::new(
                    TirExprKind::StringLiteral(self.current_module_source.to_string()),
                    string_type,
                    lit.span,
                );
            }
            ast::Literal::LocationLine => {
                // `#line` — 1-indexed line number; matches the
                // elaborator's `I32` typing.
                let line = lit.span.line as u64;
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    crate::tir::TypeTable::I32,
                    lit.span,
                );
            }
            ast::Literal::DataSection => {
                // `#data` — the loaded module's `__DATA__` section.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                let data = self
                    .loaded_modules
                    .get(&self.current_module_source)
                    .and_then(|m| m.data_section())
                    .map(str::to_owned)
                    .unwrap_or_default();
                return TirExpr::new(TirExprKind::StringLiteral(data), string_type, lit.span);
            }
            ast::Literal::IncludeStr(raw_path) => {
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let value = self
                    .tysys
                    .included_files
                    .get(&key)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .map(str::to_owned)
                    .unwrap_or_default();
                return TirExpr::new(TirExprKind::StringLiteral(value), string_type, lit.span);
            }
            ast::Literal::IncludeBytes(raw_path) => {
                let array_u8_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_array(crate::tir::TypeTable::U8);
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let bytes = self
                    .tysys
                    .included_files
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                return TirExpr::new(TirExprKind::BytesLiteral(bytes), array_u8_type, lit.span);
            }
        };
        TirExpr::new(kind, recorded_type, lit.span)
    }

    /// Reify a pattern in a `let`, `match`, `if let`, or `while let`.
    /// Binding patterns add locals to `ctx` in the same order annotate
    /// did (per the walk-order invariant). The variant binding order
    /// mirrors `Elaborator::resolve_if_pattern_inner`'s recursion.
    /// Resolve a nullary qualified pattern (`TokenKind::FOO`, `i32::MAX`)
    /// to its associated-constant value, mirroring the elaborator's
    /// pattern lowering (stmt.rs:1428+). Integer constants become
    /// `Literal` patterns (signed/unsigned per the scrutinee) so they
    /// benefit from switch lowering; everything else becomes a
    /// `ConstantValue`. Returns `None` when the name is not a recorded
    /// associated constant (i.e. it is a real variant case).
    fn reify_associated_const_pattern(
        &mut self,
        variant_name: &str,
        variant_qualifier: Option<&ast::Type>,
        scrutinee_type: TypeId,
        span: crate::token::Span,
        ctx: &mut FunctionContext,
    ) -> Option<TirPattern> {
        use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirLiteralPattern};

        // Key matches `Elaborator::format_assoc_const_key`: bare name when
        // unqualified, else `<base>::<name>` using the qualifier's base
        // type name.
        let key = match variant_qualifier {
            None => variant_name.to_string(),
            Some(ast::Type::Named(t)) => format!("{}::{}", t.name, variant_name),
            Some(ast::Type::Generic(t)) => format!("{}::{}", t.name, variant_name),
            Some(ast::Type::NamespacedGeneric(t)) => format!("{}::{}", t.name, variant_name),
            Some(_) => variant_name.to_string(),
        };

        let (const_module, const_ty, const_expr) =
            self.sem.decls.associated_constants.get(&key).cloned()?;

        let type_id = self.resolve_type(&const_ty);
        // Reify the body under its defining module so colliding cross-module
        // `AstId`s can't mis-type the inlined constant (see `reify_ident`).
        let resolved = self.with_const_module_perspective(&const_module, |this| {
            this.reify_expr(&const_expr, ctx, Some(type_id))
        });
        match &resolved.kind {
            TirExprKind::IntLiteral { repr, .. } => {
                let is_unsigned = matches!(
                    self.tysys.type_table.borrow().get(scrutinee_type),
                    ResolvedType::Primitive(
                        crate::tir::PrimitiveType::U8
                            | crate::tir::PrimitiveType::U16
                            | crate::tir::PrimitiveType::U32
                            | crate::tir::PrimitiveType::U64
                            | crate::tir::PrimitiveType::U128
                    ),
                ) || matches!(
                    self.tysys.type_table.borrow().get(scrutinee_type),
                    ResolvedType::Struct { name, .. } if name == "u128",
                );
                if is_unsigned {
                    if let Ok(v) = super::util::parse_u128_literal(repr) {
                        return Some(TirPattern::Literal(TirLiteralPattern::U128(v)));
                    }
                } else if let Ok(v) = super::util::parse_i128_literal(repr) {
                    return Some(TirPattern::Literal(TirLiteralPattern::I128(v)));
                }
            }
            TirExprKind::BoolLiteral(v) => {
                return Some(TirPattern::Literal(TirLiteralPattern::Bool(*v)));
            }
            TirExprKind::CharLiteral(v) => {
                return Some(TirPattern::Literal(TirLiteralPattern::Char(*v)));
            }
            _ => {}
        }
        Some(TirPattern::ConstantValue {
            expr: Box::new(TirExpr::new(resolved.kind, type_id, span)),
        })
    }

    /// Resolve a range-pattern endpoint to its `i128` value. Literal
    /// endpoints parse directly; an associated-constant endpoint
    /// (`i32::MIN`, `TokenKind::FOO`) resolves through
    /// `sem.decls.associated_constants` — mirroring the elaborator, which
    /// inlines const range bounds to their values.
    fn pattern_endpoint_value(
        &mut self,
        endpoint: &ast::Pattern,
        ctx: &mut FunctionContext,
    ) -> i128 {
        use crate::tir::TirExprKind;
        if let ast::Pattern::Variant {
            variant_name,
            variant_qualifier,
            bindings,
            ..
        } = endpoint
            && bindings.is_empty()
        {
            // Builtin primitive const (`i32::MIN`, `u8::MAX`): not in the
            // user `associated_constants` map, resolved by value.
            if let Some(v) =
                super::stmt::primitive_assoc_const_to_i128(variant_qualifier.as_ref(), variant_name)
            {
                return v;
            }
            let key = match variant_qualifier {
                None => variant_name.clone(),
                Some(ast::Type::Named(t)) => format!("{}::{}", t.name, variant_name),
                Some(ast::Type::Generic(t)) => format!("{}::{}", t.name, variant_name),
                Some(ast::Type::NamespacedGeneric(t)) => format!("{}::{}", t.name, variant_name),
                Some(_) => variant_name.clone(),
            };
            if let Some((const_module, const_ty, const_expr)) =
                self.sem.decls.associated_constants.get(&key).cloned()
            {
                let type_id = self.resolve_type(&const_ty);
                let resolved = self.with_const_module_perspective(&const_module, |this| {
                    this.reify_expr(&const_expr, ctx, Some(type_id))
                });
                if let TirExprKind::IntLiteral { repr, .. } = &resolved.kind {
                    return super::util::parse_i128_literal(repr)
                        .or_else(|_| super::util::parse_u128_literal(repr).map(|v| v as i128))
                        .unwrap_or(0);
                }
            }
        }
        pattern_endpoint_to_i128(endpoint)
    }

    /// Discriminant index of `case_name` when `scrutinee_type` is an
    /// enum that declares it. Drives lowering a bare/qualified enum-case
    /// pattern to `TirPattern::Enum`.
    fn scrutinee_enum_case_index(&self, scrutinee_type: TypeId, case_name: &str) -> Option<u32> {
        use crate::tir::ResolvedType;
        // Peel references for match ergonomics: `match &c { Red => … }`
        // presents the scrutinee as `&Color`.
        let peeled = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
        let decl_name = match self.tysys.type_table.borrow().get(peeled).clone() {
            ResolvedType::Enum { name, .. } => name,
            _ => return None,
        };
        let lookup = self.type_lookup();
        lookup
            .enum_case(&decl_name)?
            .case_index
            .get(case_name)
            .copied()
    }

    /// True when `scrutinee_type` is a variant (directly or as a generic
    /// instance) whose cases include `case_name`.
    fn scrutinee_has_variant_case(&self, scrutinee_type: TypeId, case_name: &str) -> bool {
        use crate::tir::ResolvedType;
        let peeled = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
        let decl_name = match self.tysys.type_table.borrow().get(peeled).clone() {
            ResolvedType::Variant { name, .. } | ResolvedType::GenericInstance { name, .. } => name,
            _ => return false,
        };
        self.type_lookup()
            .variant_case(&decl_name)
            .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name))
    }

    /// Lower a nullary variant-case pattern (e.g. `None`) to
    /// `TirPattern::Variant` with no bindings, resolving the case's
    /// payload type from the scrutinee's variant decl. Shared by the
    /// bare-ident and qualified-`Variant` arms.
    fn reify_nullary_variant_case(
        &mut self,
        scrutinee_type: TypeId,
        case_name: &str,
    ) -> TirPattern {
        use crate::tir::ResolvedType;
        // Peel references (match ergonomics): `if let None = rn` with
        // `rn: &Option<T>` matches a nullary case through the reference.
        let peeled = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
        let (decl_name, type_args) = match self.tysys.type_table.borrow().get(peeled).clone() {
            ResolvedType::Variant { name, .. } => (name, Vec::<TypeId>::new()),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => (name, type_args),
            _ => (String::new(), Vec::new()),
        };
        let payload_type = self.get_variant_case_payload_type(&decl_name, case_name, &type_args);
        TirPattern::Variant {
            enum_type: peeled,
            variant_name: case_name.to_string(),
            bindings: vec![],
            payload_type,
        }
    }

    /// A bare ident naming an immutable global lowers to a
    /// `ConstantValue` comparison against that global rather than a
    /// binding (mirrors `Elaborator::resolve_if_pattern_inner`,
    /// stmt.rs:1290+). Mutable globals are not constants and fall through
    /// to a binding.
    fn reify_immutable_global_pattern(
        &self,
        name: &str,
        span: crate::token::Span,
    ) -> Option<TirPattern> {
        use crate::tir::{TirExpr, TirExprKind};
        if let Some(&(ty, mutable)) = self.sem.decls.current_module_globals.get(name)
            && !mutable
        {
            return Some(TirPattern::ConstantValue {
                expr: Box::new(TirExpr::new(
                    TirExprKind::GlobalVarGet {
                        module_source: self.current_module_source.clone(),
                        name: name.to_string(),
                    },
                    ty,
                    span,
                )),
            });
        }
        if let Some((source_module, original_name, ty, mutable)) =
            self.sem.decls.imported_globals.get(name)
            && !*mutable
        {
            return Some(TirPattern::ConstantValue {
                expr: Box::new(TirExpr::new(
                    TirExprKind::GlobalVarGet {
                        module_source: source_module.clone(),
                        name: original_name.clone(),
                    },
                    *ty,
                    span,
                )),
            });
        }
        None
    }

    /// Wrap `inner` in the reference kind of `scrutinee_type` for match
    /// ergonomics. Walks the reference layers of the scrutinee: a `&mut`
    /// sets `&mut` unless a `&` is also present (most restrictive wins),
    /// matching `Elaborator::resolve_if_pattern`'s `RefBinding`. A
    /// non-reference scrutinee returns `inner` unchanged.
    fn apply_scrutinee_ref_kind(&self, scrutinee_type: TypeId, inner: TypeId) -> TypeId {
        use crate::tir::ResolvedType;
        let mut cur = scrutinee_type;
        let mut saw_ref = false;
        let mut saw_mut_ref = false;
        loop {
            let resolved = self.tysys.type_table.borrow().get(cur).clone();
            match resolved {
                ResolvedType::Ref(i) => {
                    saw_ref = true;
                    cur = i;
                }
                ResolvedType::MutRef(i) => {
                    saw_mut_ref = true;
                    cur = i;
                }
                _ => break,
            }
        }
        if saw_ref {
            self.tysys.type_table.borrow_mut().make_ref(inner)
        } else if saw_mut_ref {
            self.tysys.type_table.borrow_mut().make_mut_ref(inner)
        } else {
            inner
        }
    }

    pub(super) fn reify_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Wildcard => TirPattern::Wildcard,
            ast::Pattern::Ident { id, name, span } => {
                // A bare ident in a pattern is ambiguous: a nullary
                // enum/variant case (`None`, `Red`), an immutable global
                // constant, or a fresh binding. Disambiguate in the same
                // order as `Elaborator::resolve_if_pattern_inner`
                // (stmt.rs:1255+): known case first, then immutable
                // global, then binding.
                if let Some(case_index) = self.scrutinee_enum_case_index(scrutinee_type, name) {
                    return TirPattern::Enum {
                        enum_type: self.tysys.type_table.borrow().peel_refs(scrutinee_type),
                        case_name: name.clone(),
                        case_index,
                    };
                }
                if self.scrutinee_has_variant_case(scrutinee_type, name) {
                    return self.reify_nullary_variant_case(scrutinee_type, name);
                }
                if let Some(const_pat) = self.reify_immutable_global_pattern(name, *span) {
                    return const_pat;
                }
                let local_index = ctx.add_local(
                    name.clone(),
                    scrutinee_type,
                    /* is_mut */ false,
                    Some(*id),
                );
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id: scrutinee_type,
                }
            }
            ast::Pattern::MutIdent { id, name, span: _ } => {
                let local_index = ctx.add_local(
                    name.clone(),
                    scrutinee_type,
                    /* is_mut */ true,
                    Some(*id),
                );
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id: scrutinee_type,
                }
            }
            ast::Pattern::Literal(lit) => {
                use crate::tir::{PrimitiveType, ResolvedType, TirLiteralPattern};
                // Mirror `Elaborator::resolve_if_pattern_inner`'s literal
                // arm (stmt.rs:1344): wide-int literals follow the
                // scrutinee's signedness (a `u128` scrutinee must compare
                // via `u128::*`, not `i128::*`, or codegen emits a
                // `(ref $u128)` vs `(ref $i128)` mismatch), and char /
                // string literals decode their escapes. `null` on a
                // variant scrutinee with a `None` case lowers to that
                // case.
                let tir_lit = match lit {
                    ast::Literal::Number(repr) => {
                        let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
                        let is_unsigned = matches!(
                            resolved,
                            ResolvedType::Primitive(
                                PrimitiveType::U8
                                    | PrimitiveType::U16
                                    | PrimitiveType::U32
                                    | PrimitiveType::U64
                                    | PrimitiveType::U128
                            )
                        ) || matches!(
                            resolved,
                            ResolvedType::Struct { ref name, .. } if name == "u128"
                        );
                        if is_unsigned {
                            TirLiteralPattern::U128(
                                super::util::parse_u128_literal(repr).unwrap_or(0),
                            )
                        } else {
                            TirLiteralPattern::I128(
                                super::util::parse_i128_literal(repr).unwrap_or(0),
                            )
                        }
                    }
                    ast::Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    ast::Literal::Char(raw) => {
                        TirLiteralPattern::Char(super::util::unescape_char(raw).unwrap_or('\0'))
                    }
                    ast::Literal::String(raw) => TirLiteralPattern::String(
                        super::util::unescape_string(raw).unwrap_or_default(),
                    ),
                    ast::Literal::Null => {
                        if self.scrutinee_has_variant_case(scrutinee_type, "None") {
                            return self.reify_nullary_variant_case(scrutinee_type, "None");
                        }
                        TirLiteralPattern::Null
                    }
                    _ => ast_literal_to_pattern(lit),
                };
                TirPattern::Literal(tir_lit)
            }
            ast::Pattern::Tuple(elements, has_rest) => {
                // Tuple patterns destructure into the scrutinee's
                // element types. The elaborator already validated
                // arity; reify reads `tysys.type_table.as_tuple` to
                // get the per-element types, falling back to
                // UNKNOWN-typed inner walks for type-pack scrutinees.
                // Destructuring through a reference (`let [a, b] = &t`)
                // peels the ref for the element lookup, and each element
                // binding inherits the reference kind (match ergonomics).
                let peeled = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
                let elem_types: Vec<TypeId> = self
                    .tysys
                    .type_table
                    .borrow()
                    .as_tuple(peeled)
                    .unwrap_or_default();
                let sub_patterns: Vec<TirPattern> = elements
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let elem_ty = elem_types
                            .get(i)
                            .copied()
                            .unwrap_or(crate::tir::TypeTable::UNKNOWN);
                        let binding_ty = self.apply_scrutinee_ref_kind(scrutinee_type, elem_ty);
                        self.reify_pattern(p, binding_ty, ctx)
                    })
                    .collect();
                TirPattern::Tuple(sub_patterns, *has_rest)
            }
            ast::Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                span,
                ..
            } => {
                // Associated-constant pattern (`TokenKind::FOO`,
                // `i32::MAX`): a nullary qualified name that resolves to a
                // recorded associated constant rather than a variant case.
                // The elaborator inlines it to the constant's value
                // (stmt.rs:1428+); reify reproduces the same lowering from
                // `sem.decls.associated_constants`. Real variant cases are
                // never in that map, so the lookup distinguishes the two.
                if bindings.is_empty()
                    && let Some(const_pat) = self.reify_associated_const_pattern(
                        variant_name,
                        variant_qualifier.as_ref(),
                        scrutinee_type,
                        *span,
                        ctx,
                    )
                {
                    return const_pat;
                }

                // Variant patterns appear in `match Some(x) { Some(v) => …
                // }` etc. The case's payload type lives on
                // `tysys.all_variant_cases`; reify reads it to give
                // sub-patterns the right scrutinee type. The
                // `variant_name` strips a `Variant::` prefix when
                // present (the AST keeps the qualified form).
                let case_name = variant_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(variant_name)
                    .to_string();

                // Enum-case pattern (plain discriminant, no payload):
                // `Color::Red`. The elaborator emits `TirPattern::Enum`
                // with the case's discriminant index (stmt.rs:1562+);
                // reify reproduces it when the scrutinee is an enum.
                if let Some(case_index) = self.scrutinee_enum_case_index(scrutinee_type, &case_name)
                {
                    return TirPattern::Enum {
                        enum_type: self.tysys.type_table.borrow().peel_refs(scrutinee_type),
                        case_name,
                        case_index,
                    };
                }

                // Resolve the variant decl + case payload. A method that
                // takes `&self` matches on a reference (`match self { … }`
                // where `self: &Option<T>`); peel references so the
                // variant decl + payload type resolve through the
                // underlying `Option<T>` rather than falling to the
                // unknown-payload `_` arm.
                let peeled_scrutinee = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
                let (payload_type, _payload_decl_module) = {
                    use crate::tir::ResolvedType;
                    let resolved = self.tysys.type_table.borrow().get(peeled_scrutinee).clone();
                    let (decl_name, type_args) = match resolved {
                        ResolvedType::Variant {
                            name,
                            module_source,
                        } => (name, (Vec::<TypeId>::new(), module_source)),
                        ResolvedType::GenericInstance {
                            name,
                            module_source,
                            type_args,
                        } => (name, (type_args, module_source)),
                        _ => (
                            String::new(),
                            (Vec::new(), self.current_module_source.clone()),
                        ),
                    };
                    let payload =
                        self.get_variant_case_payload_type(&decl_name, &case_name, &type_args.0);
                    (payload, type_args.1)
                };

                // Match ergonomics: when the scrutinee is a reference
                // (`match self { Some(v) => … }` with `self: &Option<T>`),
                // the payload binding inherits the reference kind — `v` is
                // `&T`, not `T`, so it forwards directly to a `&self`
                // method. Mirrors `Elaborator::resolve_if_pattern`'s
                // `RefBinding` handling (stmt.rs:1213+): `&` downgrades a
                // `&mut` (most restrictive wins). The `payload_type` field
                // and `enum_type` stay the unwrapped (peeled) forms — the
                // variant extraction reads the value through the peeled
                // variant type; only the binding scrutinee carries the
                // reference.
                let binding_scrutinee = self.apply_scrutinee_ref_kind(scrutinee_type, payload_type);
                let sub_patterns: Vec<TirPattern> = bindings
                    .iter()
                    .map(|p| self.reify_pattern(p, binding_scrutinee, ctx))
                    .collect();
                TirPattern::Variant {
                    enum_type: peeled_scrutinee,
                    variant_name: case_name,
                    bindings: sub_patterns,
                    payload_type,
                }
            }
            ast::Pattern::Or(alternatives) => {
                // Or patterns match any alternative. Each alternative
                // binds the same names, but a naive per-alternative walk
                // gives each its own local slot — so `Num(n) | Neg(n)`
                // would extract the payload into one slot and the arm body
                // read another. Mirror `resolve_if_pattern_inner`
                // (stmt.rs:1798): remap each later alternative's binding
                // locals onto the first alternative's, then point the
                // arm-scope bindings at the first alternative's locals.
                let mut resolved: Vec<TirPattern> = Vec::with_capacity(alternatives.len());
                if let Some(first_alt) = alternatives.first() {
                    let first = self.reify_pattern(first_alt, scrutinee_type, ctx);
                    let first_bindings = super::stmt::collect_pattern_bindings_with_index(&first);
                    resolved.push(first);

                    for alt in alternatives.iter().skip(1) {
                        let alt_resolved = self.reify_pattern(alt, scrutinee_type, ctx);
                        let alt_bindings =
                            super::stmt::collect_pattern_bindings_with_index(&alt_resolved);
                        let mut remapped = alt_resolved;
                        for (first_bind, alt_bind) in first_bindings.iter().zip(alt_bindings.iter())
                        {
                            if first_bind.1 != alt_bind.1 {
                                super::stmt::remap_pattern_local(
                                    &mut remapped,
                                    alt_bind.1,
                                    first_bind.1,
                                );
                            }
                        }
                        resolved.push(remapped);
                    }

                    // Point the arm-scope bindings at the first
                    // alternative's locals so the body reads the slot the
                    // payload was extracted into.
                    for (name, local_index, _type_id) in &first_bindings {
                        if let Some(scope) = ctx.scopes.last_mut()
                            && let Some(var) = scope.get_mut(name)
                        {
                            var.index = *local_index;
                        }
                    }
                }
                TirPattern::Or(resolved)
            }
            ast::Pattern::Range {
                start, end, kind, ..
            } => {
                use crate::ast::RangeKind;
                use crate::tir::{PrimitiveType, ResolvedType};
                let inclusive = matches!(kind, RangeKind::Inclusive);
                let start_val = self.pattern_endpoint_value(start, ctx);
                let end_val = self.pattern_endpoint_value(end, ctx);
                let is_unsigned = matches!(
                    self.tysys.type_table.borrow().get(scrutinee_type),
                    ResolvedType::Primitive(
                        PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::U64
                            | PrimitiveType::U128,
                    )
                );
                TirPattern::Range {
                    start: start_val,
                    end: end_val,
                    inclusive,
                    is_unsigned,
                }
            }
            ast::Pattern::Struct {
                type_name,
                fields,
                has_rest,
                ..
            } => self.reify_struct_pattern(
                type_name.as_deref(),
                fields,
                *has_rest,
                scrutinee_type,
                ctx,
            ),
        }
    }

    /// Reify a struct destructuring pattern `Point { x, y }` or
    /// `{ x, y }` (anonymous). The struct's field-name → index map
    /// comes from `tysys.all_struct_fields`; sub-patterns recurse
    /// against the declared field type. Mirrors
    /// `Elaborator::resolve_struct_pattern`'s shape; shorthand
    /// `{ x }` (== `{ x: x }`) is encoded by the AST having the
    /// sub-pattern be an `Ident { name: x }` either way.
    fn reify_struct_pattern(
        &mut self,
        type_name: Option<&str>,
        fields: &[ast::StructPatternField],
        has_rest: bool,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        use crate::tir::{ResolvedType, TirStructPatternField};

        // Determine the struct name: explicit `Type::Pattern` wins;
        // otherwise read from the scrutinee.
        // Destructuring through a reference (`let { x, y } = &p`)
        // presents the scrutinee as `&Point`; peel references so the
        // struct decl resolves (fields inherit the reference kind below).
        let peeled_scrutinee = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
        let scrutinee_struct_name =
            match self.tysys.type_table.borrow().get(peeled_scrutinee).clone() {
                ResolvedType::Struct { name, .. } => name,
                ResolvedType::GenericInstance { name, .. } => name,
                _ => String::new(),
            };
        let lookup_name = type_name.unwrap_or(&scrutinee_struct_name);

        // Decl-interned struct info for field-name → (index, type)
        // lookup. Falls back to UNKNOWN-typed sub-patterns for
        // anonymous / unresolved scrutinees (matches annotate's
        // recovery shape).
        let field_info: crate::hashmap::IndexMap<String, (u32, TypeId)> = {
            let lookup = self.type_lookup();
            lookup
                .struct_fields(lookup_name)
                .map(|info| {
                    info.fields
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t, _))| (n.clone(), (i as u32, *t)))
                        .collect()
                })
                .unwrap_or_default()
        };

        let tir_fields: Vec<TirStructPatternField> = fields
            .iter()
            .map(|f| {
                let (field_index, field_ty) = field_info
                    .get(&f.field_name)
                    .copied()
                    .unwrap_or((0, crate::tir::TypeTable::UNKNOWN));
                // Match ergonomics: a field bound through a `&Point` /
                // `&mut Point` scrutinee is `&field` / `&mut field`.
                let binding_ty = self.apply_scrutinee_ref_kind(scrutinee_type, field_ty);
                let pattern = self.reify_pattern(&f.pattern, binding_ty, ctx);
                TirStructPatternField {
                    field_name: f.field_name.clone(),
                    field_index,
                    pattern,
                }
            })
            .collect();

        TirPattern::Struct {
            struct_type: scrutinee_type,
            fields: tir_fields,
            has_rest,
        }
    }

    /// Look up a variant case's payload type, substituted with the
    /// scrutinee's type args. Reify-side mirror of the elaborator's
    /// `Elaborator::get_variant_case_payload_type`; the lookup walks
    /// `tysys.all_variant_cases` and the local-module override map
    /// via [`TypeLookup`], so the same `TypeId` annotate produced
    /// lands on the reified pattern.
    fn get_variant_case_payload_type(
        &self,
        variant_name: &str,
        case_name: &str,
        type_args: &[TypeId],
    ) -> TypeId {
        let lookup = self.type_lookup();
        let (payload, type_param_indices): (TypeId, Vec<u32>) = {
            let Some(variant_info) = lookup.variant_case(variant_name) else {
                return crate::tir::TypeTable::UNKNOWN;
            };
            let Some(case_data) = variant_info.cases.iter().find(|c| c.name == case_name) else {
                return crate::tir::TypeTable::UNKNOWN;
            };
            // Extract the variant decl's type-param indices so the
            // substitution map below is keyed by `index` — matching
            // `TypeTable::substitute_type_params` (tir.rs:1480).
            let indices: Vec<u32> = (0..variant_info.type_param_type_ids.len() as u32).collect();
            (case_data.payload, indices)
        };
        drop(lookup);
        if type_args.is_empty() {
            return payload;
        }
        // Map TypeParam{index} → concrete `type_args[index]`. Recurse
        // through containers (`Ref`, `BuiltinArray`, `GenericInstance`,
        // `Function`, …) via `TypeTable::substitute_type_params`.
        let substitution: crate::hashmap::IndexMap<u32, TypeId> = type_param_indices
            .iter()
            .zip(type_args.iter())
            .map(|(&idx, &t)| (idx, t))
            .collect();
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(payload, &substitution)
    }
}

/// Decode a range-pattern endpoint (`a..<b` / `a..=b`) into its
/// `i128` value. The endpoint syntactic form is itself a `Pattern`:
/// either a `Literal(Number)` or a `Literal(Char)` (for char-range
/// patterns). Char endpoints lower to their codepoint as `i128`;
/// numeric endpoints reuse the same hex / oct / bin recogniser as
/// `ast_literal_to_pattern`'s integer decode. Non-literal endpoints
/// are a parser-elaborator invariant violation — annotate has already
/// diagnosed them — so reify panics with a labelled tripwire.
fn pattern_endpoint_to_i128(endpoint: &ast::Pattern) -> i128 {
    match endpoint {
        ast::Pattern::Literal(ast::Literal::Number(repr)) => {
            let digits = repr.replace('_', "");
            if let Some(stripped) = digits.strip_prefix("0x") {
                i128::from_str_radix(stripped, 16).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0o") {
                i128::from_str_radix(stripped, 8).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0b") {
                i128::from_str_radix(stripped, 2).unwrap_or(0)
            } else {
                digits.parse::<i128>().unwrap_or(0)
            }
        }
        ast::Pattern::Literal(ast::Literal::Char(s)) => {
            let inner = s.trim_start_matches('\'').trim_end_matches('\'');
            i128::from(inner.chars().next().unwrap_or('\0') as u32)
        }
        _ => panic!(
            "pattern_endpoint_to_i128: non-literal range endpoint {endpoint:?} \
             (annotate should have diagnosed)"
        ),
    }
}

/// Map an AST [`ast::Literal`] in pattern position to its
/// [`crate::tir::TirLiteralPattern`] counterpart. Number literals
/// decode into `I128` (parsed via the same hex / oct / bin prefix
/// recogniser used by `reify_literal`), with negative sources kept
/// as their parsed numeric value. The `Null` / `Unit` literals never
/// appear in pattern position in the surface grammar — they panic
/// here to surface a parser-elaborator invariant violation early.
fn ast_literal_to_pattern(lit: &ast::Literal) -> crate::tir::TirLiteralPattern {
    use crate::tir::TirLiteralPattern;
    match lit {
        ast::Literal::Number(repr) => {
            // Mirror `reify_literal`'s numeric decode: prefer
            // hex/oct/bin radix, else decimal. Pattern position
            // never sees float literals (the elaborator rejects
            // them earlier), so decode as integer.
            let digits = repr.replace('_', "");
            let value: i128 = if let Some(stripped) = digits.strip_prefix("0x") {
                i128::from_str_radix(stripped, 16).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0o") {
                i128::from_str_radix(stripped, 8).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0b") {
                i128::from_str_radix(stripped, 2).unwrap_or(0)
            } else {
                digits.parse::<i128>().unwrap_or(0)
            };
            TirLiteralPattern::I128(value)
        }
        ast::Literal::String(s) => TirLiteralPattern::String(s.clone()),
        ast::Literal::Char(s) => {
            let inner = s.trim_start_matches('\'').trim_end_matches('\'');
            TirLiteralPattern::Char(inner.chars().next().unwrap_or('\0'))
        }
        ast::Literal::Bool(b) => TirLiteralPattern::Bool(*b),
        ast::Literal::Null => TirLiteralPattern::Null,
        // Unit / Location / Include literals don't appear as pattern
        // literals in the surface grammar — the parser rejects them
        // earlier. Falling here would be a parser-elaborator
        // invariant violation; panic with a labelled tripwire.
        ast::Literal::Unit
        | ast::Literal::LocationFile
        | ast::Literal::LocationLine
        | ast::Literal::LocationFunction
        | ast::Literal::DataSection
        | ast::Literal::IncludeStr(_)
        | ast::Literal::IncludeBytes(_) => {
            panic!("ast_literal_to_pattern: literal kind {lit:?} not valid in pattern position")
        }
    }
}

/// Map an AST [`ast::UnaryOp`] to its TIR counterpart. The two enums
/// are 1:1; this helper exists so the dispatch table doesn't repeat
/// the mapping at every Unary arm.
/// Free-function attribute extractors — mirror
/// `Elaborator::extract_*` (item.rs:802+). The elaborator's
/// instance methods take only `&[Attribute]`, so we reproduce
/// them as free functions so reify can call them without holding
/// an Elaborator.
fn extract_is_ambient_attr(attrs: &[crate::ast::Attribute]) -> bool {
    attrs.iter().any(|a| a.name == "ambient")
}

fn extract_inline_hint_attr(attrs: &[crate::ast::Attribute]) -> crate::tir::InlineHint {
    let Some(attr) = attrs.iter().find(|a| a.name == "inline") else {
        return crate::tir::InlineHint::Auto;
    };
    match attr.args.first().map(crate::ast::AttrArg::as_str) {
        Some("always") => crate::tir::InlineHint::Always,
        Some("never") => crate::tir::InlineHint::Never,
        None => crate::tir::InlineHint::Hint,
        _ => crate::tir::InlineHint::Auto,
    }
}

fn extract_export_name_attr(attrs: &[crate::ast::Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == "export_name")
        .and_then(|a| a.args.first())
        .map(|a| a.as_str().to_string())
}

fn extract_allocator_tag_attr(attrs: &[crate::ast::Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == "allocator")
        .and_then(|a| a.args.first())
        .map(|a| a.as_str().to_string())
}

/// Return the base name of an AST [`ast::Type`] for impl-block /
/// method-name mangling. A free-function variant matching the
/// elaborator's `Elaborator::get_type_name_static` (module.rs).
/// Used by `Reify::find_from_impl_module` to recognise an
/// `impl From<Source> for Target` block by its AST shape.
fn ast_type_name_static(ty: &ast::Type) -> String {
    use crate::tir::TypeTable;
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
        ast::Type::Function(_)
        | ast::Type::NamespacedGeneric(_)
        | ast::Type::TypePackSpread(_, _) => "Unknown".to_string(),
    }
}

/// True when `arg` is a closure literal with at least one param that lacks a
/// type annotation. Reify forwards the recorded callee param type as the
/// closure's expected type only in this case: it is what lets an unannotated
/// `|a, b| ...` infer its params from a `fn`-typed (or `fn`-newtype) param.
/// Closures whose params are fully annotated (or take no params) gain nothing
/// and must not receive the expected type — doing so would pin an
/// effect-polymorphic closure's `declared_effects` to a generic effect param
/// instead of the effects inferred from its body.
fn arg_is_unannotated_closure(arg: &ast::Expr) -> bool {
    matches!(arg, ast::Expr::Closure(c) if c.params.iter().any(|p| p.ty.is_none()))
}

/// Compile-time value and primitive `TypeId` for a primitive integer
/// associated constant named `<prefix>::<suffix>` (e.g. `i32::MAX`).
/// Returns `None` for non-primitive or unknown constants. Used by
/// `reify_ident` to resolve such constants when they are not present in
/// `associated_constants` — e.g. a default-argument expression reified
/// under a stdlib-snapshot callee module whose `associated_constants` map
/// was not rehydrated. The value table mirrors
/// [`super::stmt::primitive_assoc_const_to_i128`].
fn primitive_int_assoc_const(prefix: &str, suffix: &str) -> Option<(i128, crate::tir::TypeId)> {
    use crate::tir::TypeTable;
    let ty = match prefix {
        "i8" => TypeTable::I8,
        "i16" => TypeTable::I16,
        "i32" => TypeTable::I32,
        "i64" => TypeTable::I64,
        "u8" => TypeTable::U8,
        "u16" => TypeTable::U16,
        "u32" => TypeTable::U32,
        "u64" => TypeTable::U64,
        _ => return None,
    };
    let value = match (prefix, suffix) {
        ("i8", "MAX") => i128::from(i8::MAX),
        ("i8", "MIN") => i128::from(i8::MIN),
        ("i16", "MAX") => i128::from(i16::MAX),
        ("i16", "MIN") => i128::from(i16::MIN),
        ("i32", "MAX") => i128::from(i32::MAX),
        ("i32", "MIN") => i128::from(i32::MIN),
        ("i64", "MAX") => i128::from(i64::MAX),
        ("i64", "MIN") => i128::from(i64::MIN),
        ("u8", "MAX") => i128::from(u8::MAX),
        ("u8", "MIN") => i128::from(u8::MIN),
        ("u16", "MAX") => i128::from(u16::MAX),
        ("u16", "MIN") => i128::from(u16::MIN),
        ("u32", "MAX") => i128::from(u32::MAX),
        ("u32", "MIN") => i128::from(u32::MIN),
        ("u64", "MAX") => i128::from(u64::MAX),
        ("u64", "MIN") => i128::from(u64::MIN),
        _ => return None,
    };
    Some((value, ty))
}

fn ast_unary_op_to_tir(op: ast::UnaryOp) -> crate::tir::TirUnaryOp {
    use crate::tir::TirUnaryOp;
    match op {
        ast::UnaryOp::Neg => TirUnaryOp::Neg,
        ast::UnaryOp::Not => TirUnaryOp::Not,
        ast::UnaryOp::BitNot => TirUnaryOp::BitNot,
        ast::UnaryOp::Ref => TirUnaryOp::Ref,
        ast::UnaryOp::MutRef => TirUnaryOp::MutRef,
        ast::UnaryOp::Deref => TirUnaryOp::Deref,
    }
}

/// Map an AST [`ast::BinaryOp`] to its TIR counterpart. The mapping is
/// 1:1 for the source-level ops; TIR adds `RefEq` / `RefNotEq` as
/// internal variants that the elaborator only synthesises after
/// coercion analysis, so reify never produces them from this helper.
fn ast_binary_op_to_tir(op: ast::BinaryOp) -> crate::tir::TirBinaryOp {
    use crate::tir::TirBinaryOp;
    match op {
        ast::BinaryOp::Add => TirBinaryOp::Add,
        ast::BinaryOp::Sub => TirBinaryOp::Sub,
        ast::BinaryOp::Mul => TirBinaryOp::Mul,
        ast::BinaryOp::Div => TirBinaryOp::Div,
        ast::BinaryOp::Mod => TirBinaryOp::Mod,
        ast::BinaryOp::Eq => TirBinaryOp::Eq,
        ast::BinaryOp::NotEq => TirBinaryOp::NotEq,
        ast::BinaryOp::Lt => TirBinaryOp::Lt,
        ast::BinaryOp::LtEq => TirBinaryOp::LtEq,
        ast::BinaryOp::Gt => TirBinaryOp::Gt,
        ast::BinaryOp::GtEq => TirBinaryOp::GtEq,
        ast::BinaryOp::And => TirBinaryOp::And,
        ast::BinaryOp::Or => TirBinaryOp::Or,
        ast::BinaryOp::BitAnd => TirBinaryOp::BitAnd,
        ast::BinaryOp::BitOr => TirBinaryOp::BitOr,
        ast::BinaryOp::BitXor => TirBinaryOp::BitXor,
        ast::BinaryOp::Shl => TirBinaryOp::Shl,
        ast::BinaryOp::Shr => TirBinaryOp::Shr,
    }
}
