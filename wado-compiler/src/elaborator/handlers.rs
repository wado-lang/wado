//! Elaborator lowering for effect handler installation (`with E => h do { ... }`)
//! and the `resume` control-flow expression.
//!
//! See WEP 2026-04-11 (Effect Handler) for the language semantics.
//!
//! Validation responsibilities here:
//! - Each binding's effect name must resolve to a known effect declaration
//!   (not a regular trait, struct, or unknown name).
//! - The handler value's underlying type (after stripping `&` / `&mut`) must
//!   have an `impl <Effect> for <Type>` block in scope.
//! - `resume` is only valid inside a handler method body. The actual
//!   type-check of the resume value against the operation's return type is
//!   performed by the existing return-type checker (resume lowers to
//!   `Return { value }` in the dispatch synthesis pass).
//!
//! Bundled handlers (`with &mut h do`) expand at this layer to one
//! `TirHandlerBinding` per effect that the handler's underlying type
//! implements. Each generated binding goes through the same downstream
//! dispatch-synthesis path as an explicit `Effect => h` form, and bindings
//! are emitted in **source order** — both within a single bundled handler
//! (sorted by impl-block discovery order) and across bundled / explicit
//! entries on the same `with` line — so a later binding installs as the
//! inner (active) handler when the same effect appears more than once.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::tir::{
    EffectRef, ResolvedType, TirExpr, TirExprKind, TirHandlerBinding, TypeId, TypeTable,
};

use super::Elaborator;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Resolve `with E1 => h1, ... do { body }`.
    pub(super) fn resolve_with_handler(
        &mut self,
        with_expr: &ast::WithHandlerExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let mut bindings: Vec<TirHandlerBinding> = Vec::with_capacity(with_expr.handlers.len());
        // Counter feeding `TirHandlerBinding.bundle_group`. A bundled
        // clause may expand into multiple effect bindings that must all
        // share one synthesised `__h_<bundle>` local (so a value-form
        // handler is evaluated exactly once and mutations propagate
        // across effects). Allocating ids per `with` block keeps them
        // unique within one synthesis-pass invocation.
        let mut next_bundle_group: u32 = 0;

        for binding in &with_expr.handlers {
            let resolved = self.resolve_handler_binding(binding, ctx, &mut next_bundle_group);
            // A bundled binding may expand to multiple `TirHandlerBinding`s
            // (one per effect the handler type implements). They are
            // appended in the order the elaborator enumerated them, which
            // means they install in that order — earlier ones become the
            // outer handlers and later ones become inner.
            bindings.extend(resolved);
        }

        // Body is resolved in the same function context (locals introduced
        // here remain in the function frame). A new lexical scope is opened
        // so handler-introduced bindings, if any, don't leak — this matches
        // how regular block expressions behave.
        ctx.enter_scope();
        let body = self.resolve_block(&with_expr.body, ctx, None);
        ctx.exit_scope();

        // The block expression's value is currently always Unit: the parser
        // produces a statement-only body for `with ... do { ... }` and the
        // WEP does not (yet) define a value for the `with` expression.
        let result_type = TypeTable::UNIT;

        TirExpr::new(
            TirExprKind::WithHandler {
                bindings,
                body,
                result_type,
            },
            result_type,
            with_expr.span,
        )
    }

    /// Resolve a single binding inside a `with ... do` clause.
    ///
    /// Returns one `TirHandlerBinding` for the explicit form
    /// (`Effect => handler_expr`), or one binding per effect the handler's
    /// underlying type implements for the bundled form (`handler_expr`,
    /// no `=>`). Bindings are returned in install order. `next_bundle_group`
    /// is consumed (post-incremented) only when the binding expands as
    /// bundled.
    fn resolve_handler_binding(
        &mut self,
        binding: &ast::EffectHandlerBinding,
        ctx: &mut FunctionContext,
        next_bundle_group: &mut u32,
    ) -> Vec<TirHandlerBinding> {
        match &binding.effect {
            Some(effect_ty) => {
                vec![self.resolve_explicit_handler_binding(binding, effect_ty, ctx)]
            }
            None => self.resolve_bundled_handler_binding(binding, ctx, next_bundle_group),
        }
    }

    /// Resolve `Effect => handler_expr` — the explicit form.
    fn resolve_explicit_handler_binding(
        &mut self,
        binding: &ast::EffectHandlerBinding,
        effect_ty: &ast::Type,
        ctx: &mut FunctionContext,
    ) -> TirHandlerBinding {
        // Resolve the effect name. Use `resolve_effects` so that LSP
        // jump-to-def edges are recorded just like in `with E1, E2`
        // function signatures.
        let interface_name = self.get_type_name(effect_ty);
        let effect_ids = effect_ty
            .id()
            .map(|id| vec![(id, effect_ty.span())])
            .unwrap_or_default();
        let mut resolved_effects = self.resolve_effects(&[interface_name], &effect_ids);
        let effect = resolved_effects.pop();

        // The name must point at an actual effect or resource
        // declaration, not a regular trait or arbitrary identifier. Both
        // kinds are installable as handlers (see WEP 2026-04-11): the
        // `with` clause keeps the same syntax, only the dispatch wrapper
        // shape differs (resources don't declare themselves as effects on
        // the wrapper). Param effects (generic `<effect E>`) are still
        // rejected for installation: you cannot install a handler for a
        // polymorphic effect parameter.
        if let Some(eff) = &effect {
            match eff {
                EffectRef::Concrete {
                    name,
                    module_source,
                } => {
                    // `EffectRef::Concrete` already carries the canonicalised
                    // module source resolved in `resolve_effects`; build the
                    // decl-index key from it directly rather than walking the
                    // import graph a second time via `canonical_decl_key`.
                    let key = (module_source.clone(), name.clone());
                    let is_known_effect = self.tysys.trait_env.effect_decl_index.contains_key(&key);
                    let is_known_resource = self.tysys.trait_env.resource_decl_index.contains_key(&key);
                    if !is_known_effect && !is_known_resource {
                        let _ = self.logger.error(TypeError::NotAnEffect {
                            name: name.clone(),
                            span: effect_ty.span(),
                        });
                    }
                }
                EffectRef::Param { name } => {
                    let _ = self
                        .logger
                        .error(TypeError::GenericEffectParamNotInstallable {
                            name: name.clone(),
                            span: effect_ty.span(),
                        });
                }
            }
        }

        // Resolve the handler value expression in the outer scope.
        let handler = self.resolve_expr(&binding.handler, ctx, None);
        let handler_type = self.handler_underlying_type(handler.type_id);

        // Verify the underlying struct type has `impl <Effect> for <Type>`.
        if let Some(EffectRef::Concrete {
            name: interface_name,
            ..
        }) = &effect
        {
            let type_name = self.tysys.type_table.borrow().type_name(handler_type);
            // Skip the impl-presence check when the handler type is unresolved
            // (e.g. `Unknown` / `Error`), because earlier diagnostics already
            // reported the handler-side problem.
            let is_real_type = !matches!(
                self.tysys.type_table.borrow().get(handler_type),
                ResolvedType::Unknown | ResolvedType::Error
            );
            if is_real_type && !self.find_trait_impl_for_type(&type_name, interface_name) {
                let _ = self.logger.error(TypeError::HandlerEffectNotImplemented {
                    type_name,
                    interface_name: interface_name.clone(),
                    span: binding.span,
                });
            }
        }

        // Trait/resource type args at this `with E => h do` site (e.g.
        // `[u8]` for `with Stream<u8> => &mut s do`). Resolved from the
        // effect type's AST; non-generic effects produce `vec![]`. The
        // dispatch synthesis projects this together with the bare base
        // name into the per-monomorphisation `InstantiationKey`.
        let trait_type_args: Vec<TypeId> = match effect_ty {
            ast::Type::Generic(generic) => generic
                .args
                .iter()
                .map(|arg| self.resolve_type(arg))
                .collect(),
            _ => Vec::new(),
        };

        TirHandlerBinding {
            effect,
            trait_type_args,
            handler,
            handler_type,
            span: binding.span,
            bundle_group: None,
        }
    }

    /// Resolve `handler_expr` — the bundled form.
    ///
    /// Walks every `impl <Effect> for <T>` block where `T` is the handler
    /// value's underlying type, and emits one binding per effect, in
    /// impl-discovery order.
    ///
    /// Diagnostics:
    /// - If the handler underlying type implements no effect, emit
    ///   `BundledHandlerImplementsNoEffect`. The user almost certainly
    ///   meant `with E => h do`.
    /// - If the handler type is `Unknown`/`Error`, return no bindings
    ///   silently — earlier diagnostics already reported the cause.
    ///
    /// Emits `BundledHandlerUnsupportedHandlerType` for handler types
    /// that can't be index-keyed by name (type parameters, associated-
    /// type projections, function types, tuples, ...). User code can
    /// reach those cases — e.g. `fn f<T: SomeEffect>(t: &mut T) { with
    /// &mut t do { ... } }` — so a diagnostic is the right shape, not a
    /// `panic!`. The explicit `with E => h do` form remains available
    /// as the workaround.
    fn resolve_bundled_handler_binding(
        &mut self,
        binding: &ast::EffectHandlerBinding,
        ctx: &mut FunctionContext,
        next_bundle_group: &mut u32,
    ) -> Vec<TirHandlerBinding> {
        let handler = self.resolve_expr(&binding.handler, ctx, None);
        let handler_type = self.handler_underlying_type(handler.type_id);

        let resolved = self.tysys.type_table.borrow().get(handler_type).clone();
        match resolved {
            // Already-diagnosed shapes: keep quiet so we don't pile a
            // bundled-specific error on top.
            ResolvedType::Unknown | ResolvedType::Error => return Vec::new(),
            // Nameable, indexable types — proceed with enumeration.
            ResolvedType::Struct { .. }
            | ResolvedType::Enum { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Primitive(_) => {}
            // Anything else can't be the target of an `impl Effect for T`
            // block. These cases are reachable from user code — the most
            // common is a `&T` where `T` is a generic parameter — so we
            // emit a proper diagnostic instead of panicking. Returning an
            // empty bindings list lets the rest of the `with` body
            // continue resolving (the user gets one error, not a chain).
            ref other => {
                let type_name = self.tysys.type_table.borrow().type_name(handler_type);
                let type_kind = describe_resolved_type_kind(other);
                let _ = self
                    .logger
                    .error(TypeError::BundledHandlerUnsupportedHandlerType {
                        type_name,
                        type_kind,
                        span: binding.span,
                    });
                return Vec::new();
            }
        }

        let type_name = self.tysys.type_table.borrow().type_name(handler_type);
        let effects = self.collect_effect_impls_for_type(&type_name);

        if effects.is_empty() {
            let _ = self
                .logger
                .error(TypeError::BundledHandlerImplementsNoEffect {
                    type_name,
                    span: binding.span,
                });
            return Vec::new();
        }

        // All bindings expanded from this single bundled clause share one
        // `bundle_group` id, so the dispatch synthesis allocates a single
        // shared `__h_<bundle>` local. This is what makes value-form
        // `with h do { ... }` work correctly: the handler value is
        // evaluated once and every per-effect closure captures the same
        // local, so mutations from any installed effect are observed by
        // the rest. Without grouping, each binding would clone the
        // handler expression and value-form handlers would have
        // independent copies per effect.
        let bundle_group = *next_bundle_group;
        *next_bundle_group += 1;

        effects
            .into_iter()
            .map(
                |(interface_name, effect_module, type_args)| TirHandlerBinding {
                    effect: Some(EffectRef::Concrete {
                        name: interface_name,
                        module_source: effect_module,
                    }),
                    trait_type_args: type_args,
                    handler: handler.clone(),
                    handler_type,
                    span: binding.span,
                    bundle_group: Some(bundle_group),
                },
            )
            .collect()
    }

    /// Walk every `impl Trait for <type_name>` block in scope and return
    /// the `(base_trait_name, defining_module, trait_type_args)` triple
    /// for each impl whose `trait_type` resolves to an effect *or*
    /// resource declaration. Both kinds are installable as handlers
    /// (see WEP 2026-04-11), so the bundled `with h do` form expands to
    /// one binding per impl regardless of kind. The `trait_type_args`
    /// component carries the impl's instantiation (`[u8]` for
    /// `impl Stream<u8> for MockCM`, `[]` for non-generic impls) so the
    /// dispatch synthesis can route each binding to the right
    /// per-monomorphisation infrastructure.
    ///
    /// Discovery order: `trait_env.impl_index` entries first (loaded
    /// modules), then any extra impls in the current module's items
    /// that aren't covered by the index. Within each source the order
    /// matches the original module-walk order, which gives users a
    /// stable, predictable install order for bundled handlers.
    ///
    /// Duplicates are filtered by `(decl_module, base_name,
    /// trait_type_args)` — distinct instantiations of the same generic
    /// resource (`impl Stream<u8>` vs `impl Stream<i32>`) install
    /// separately; pure aliasing duplicates collapse.
    fn collect_effect_impls_for_type(
        &mut self,
        type_name: &str,
    ) -> Vec<(String, ModuleSource, Vec<TypeId>)> {
        use crate::ast::Item;
        let mut out: Vec<(String, ModuleSource, Vec<TypeId>)> = Vec::new();
        let mut seen: crate::hashmap::IndexSet<(ModuleSource, String, Vec<TypeId>)> =
            crate::hashmap::IndexSet::default();

        // Collect the impl `trait_type` ASTs first so subsequent
        // `resolve_type` calls (which need `&mut self`) don't fight the
        // borrows on `trait_env.impl_index` / `loaded_modules` /
        // `current_module_items`.
        let mut trait_types: Vec<ast::Type> = Vec::new();
        if let Some(entries) = self.tysys.trait_env.impl_index.get(type_name) {
            for (module_src, item_idx) in entries {
                let module = &self.loaded_modules[module_src];
                if let Item::Impl(impl_block) = &module.items[*item_idx]
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    trait_types.push(trait_type.clone());
                }
            }
        }
        // Impls declared in the current module aren't always reachable
        // via `impl_index` (the index is built before the current
        // module is fully resolved in some pipelines), so walk them
        // too. Mirrors `find_trait_impl_for_type`.
        for item in self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
                && Self::get_type_name_static(&impl_block.ty) == type_name
            {
                trait_types.push(trait_type.clone());
            }
        }

        for trait_type in &trait_types {
            let base_trait_name = self.get_type_name(trait_type);
            // Accept either an effect or a resource declaration. The
            // dispatch synthesis pass treats both uniformly.
            //
            // Canonicalise through the current module's import context
            // because `trait_type` was just referenced by bare name in an
            // `impl Foo for T` block we're walking; the decl index is
            // keyed by `(decl_module, name)` and two modules can declare
            // a same-named effect / resource.
            let canonical_key = self.canonical_decl_key(&base_trait_name);
            let decl_module = self
                .tysys.trait_env
                .effect_decl_index
                .get(&canonical_key)
                .map(|(m, _)| m.clone())
                .or_else(|| {
                    self.tysys.trait_env
                        .resource_decl_index
                        .get(&canonical_key)
                        .map(|(m, _)| m.clone())
                });
            let Some(decl_module) = decl_module else {
                continue;
            };
            let type_args: Vec<TypeId> = match trait_type {
                ast::Type::Generic(generic) => generic
                    .args
                    .iter()
                    .map(|arg| self.resolve_type(arg))
                    .collect(),
                _ => Vec::new(),
            };
            if seen.insert((
                decl_module.clone(),
                base_trait_name.clone(),
                type_args.clone(),
            )) {
                out.push((base_trait_name, decl_module, type_args));
            }
        }

        out
    }

    /// Strip a single leading `&` / `&mut` layer to reach the type that the
    /// handler value points at. The handler's `impl Effect for T` block is
    /// indexed by `T`, not `&T`.
    fn handler_underlying_type(&self, type_id: TypeId) -> TypeId {
        match self.tysys.type_table.borrow().get(type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => type_id,
        }
    }

    /// Resolve `resume value`.
    ///
    /// The expression itself yields `()` — `resume` is control-flow rather
    /// than a value-producing expression at the source level. The dispatch
    /// synthesis pass lowers `Resume { value }` into `Return { value }`,
    /// which is checked against the enclosing handler method's return type
    /// by the existing return-type rules.
    pub(super) fn resolve_resume(
        &mut self,
        resume: &ast::ResumeExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        if !ctx.in_handler_method {
            let _ = self
                .logger
                .error(TypeError::ResumeOutsideHandler { span: resume.span });
        }

        // Resolve the value with the surrounding method's return type as the
        // expected type so literal coercion (e.g. `resume 0` → `i64`) lines
        // up with what `return` would have produced.
        let expected = if ctx.in_handler_method {
            Some(ctx.return_type)
        } else {
            None
        };
        let value = self.resolve_expr(&resume.value, ctx, expected);

        if ctx.in_handler_method {
            self.tysys.typecheck(self.logger, value.type_id, ctx.return_type, resume.span);
        }

        TirExpr::new(
            TirExprKind::Resume {
                value: Box::new(value),
            },
            TypeTable::UNIT,
            resume.span,
        )
    }
}

/// Short label for a [`ResolvedType`] variant, used in diagnostic
/// messages emitted by the bundled-handler elaborator. Only covers the
/// shapes the elaborator rejects; the supported shapes return their own
/// canonical names via `TypeTable::type_name`.
fn describe_resolved_type_kind(ty: &ResolvedType) -> String {
    match ty {
        ResolvedType::TypeParam { .. } => "type parameter".to_string(),
        ResolvedType::AssocTypeProjection { .. } => "associated type projection".to_string(),
        ResolvedType::Function { .. } => "function type".to_string(),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => "nested reference type".to_string(),
        ResolvedType::Reactive(_) => "reactive type".to_string(),
        ResolvedType::BuiltinArray(_) => "builtin array".to_string(),
        ResolvedType::TypePack { .. } => "type pack".to_string(),
        ResolvedType::Unit => "unit".to_string(),
        ResolvedType::Never => "never".to_string(),
        // Caller already filtered out the supported variants and the
        // already-diagnosed Unknown/Error pair — anything else here is a
        // ResolvedType variant added after this function was written, so
        // surface it as "unrecognised" rather than silently mislabelling.
        _ => "unrecognised type".to_string(),
    }
}
