//! Annotation pass for effect handler installation (`with E => h do { … }`) and
//! `resume`; see WEP 2026-04-11. It validates that each binding names a real
//! effect declaration, that the handler's stripped type has an `impl` in scope,
//! and that `resume` sits in a handler method, recording only
//! `HandlerBindingFacts` for reify to rebuild the TIR nodes from.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::module_source::ModuleSource;
use crate::tir::{EffectRef, ResolvedType, TypeId, TypeTable};

use super::Elaborator;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Annotate `with E1 => h1, ... do { body }`. Walks each handler
    /// binding for fact recording + diagnostics, walks the body for
    /// fact recording, and returns a placeholder.
    ///
    /// Reify rebuilds the `WithHandler` node — its handler bindings from
    /// `HandlerBindingFacts` and its body from the AST. The combined walk's
    /// TIR is dead (the AST-level missing-return analysis in
    /// `control_flow.rs` reads `with`/`resume` off the AST, not the resolved
    /// node), so this arm records facts and projects the body's result type.
    pub(super) fn resolve_with_handler(
        &mut self,
        with_expr: &ast::WithHandlerExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Counter feeding `HandlerBindingFacts.bundle_group` for the
        // bundled-handler form. A bundled clause may expand into multiple
        // effect bindings that share one synthesised `__h_<bundle>` local
        // (value-form handler evaluated once, mutations propagate across
        // effects). Per-`with` allocation keeps ids unique inside one
        // synthesis-pass invocation.
        let mut next_bundle_group: u32 = 0;

        for binding in &with_expr.handlers {
            self.resolve_handler_binding(binding, ctx, &mut next_bundle_group);
        }

        // Body is annotated in the same function context (locals introduced
        // here remain in the function frame). A new lexical scope is opened
        // so handler-introduced bindings, if any, don't leak — matches how
        // regular block expressions behave.
        ctx.enter_scope();
        self.resolve_block_value(&with_expr.body, ctx, expected_type);
        ctx.exit_scope();

        // `with ... do { ... }` is an expression: it evaluates to its body
        // block's value. The shared block-result rule types it from the
        // body's tail — a bare expression, an `if`/`else` (with both
        // branches), a `match`, or a diverging `return` — rather than a
        // hardcoded `Unit`; that is what makes the value form
        // `let x = with E => h do { ... }` work, while the common statement
        // form (tail `Unit`) is unchanged. Read from `expression_types` (AST
        // level) so it does not depend on the body TIR.
        let result_type = self.ast_block_result_type(&with_expr.body);

        // Reify rebuilds the `WithHandler` node — its handler
        // bindings from `sem.types.handler_bindings` (recorded by
        // `resolve_handler_binding` above) and its body from the AST — so the
        // combined walk only resolves the body for its fact-recording side
        // effects and projects the result type. Missing-return analysis reads
        // `with`/`resume` off the AST via `control_flow.rs`, so nothing
        // consumes this node's structure.
        result_type
    }

    /// Annotate a single binding inside a `with ... do` clause. `bundle_group`
    /// is consumed (post-incremented) only when the binding expands as
    /// bundled. Diagnostics + fact recording are the only outputs; no TIR.
    fn resolve_handler_binding(
        &mut self,
        binding: &ast::EffectHandlerBinding,
        ctx: &mut FunctionContext,
        next_bundle_group: &mut u32,
    ) {
        match &binding.effect {
            Some(effect_ty) => {
                self.resolve_explicit_handler_binding(binding, effect_ty, ctx);
            }
            None => {
                self.resolve_bundled_handler_binding(binding, ctx, next_bundle_group);
            }
        }
    }

    /// Annotate `Effect => handler_expr` — the explicit form.
    fn resolve_explicit_handler_binding(
        &mut self,
        binding: &ast::EffectHandlerBinding,
        effect_ty: &ast::Type,
        ctx: &mut FunctionContext,
    ) {
        // Resolve the effect name. Use `resolve_effects` so that LSP
        // jump-to-def edges are recorded just like in `with (E1, E2)`
        // function signatures.
        let interface_name = self.get_type_name(effect_ty);
        let effect_ids = effect_ty
            .id()
            .map(|id| vec![(id, effect_ty.span())])
            .unwrap_or_default();
        let mut resolved_effects = self.resolve_effects(&[interface_name], &effect_ids);
        let effect = resolved_effects.pop();
        // The `with` clause names the effect in this module, and the walk
        // answered for that site — so the declaration comes from the site
        // rather than from asking a module about a spelling.
        let effect_decl = effect_ty
            .id()
            .and_then(|id| self.tysys.resolutions.declared(id));

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
                EffectRef::Concrete { name, .. } => {
                    // Ask the declaration what it is, rather than building a
                    // key and probing two indexes to find out which it belongs
                    // to.
                    let handles = effect_decl.is_some_and(|def| {
                        matches!(
                            self.tysys.resolutions.defs().kind(def),
                            crate::defs::DefKind::Effect | crate::defs::DefKind::Resource
                        )
                    });
                    if !handles {
                        let _ = self.emit(TypeError::NotAnEffect {
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
        let handler_type = self.handler_underlying_type(handler);

        // Verify the underlying struct type has `impl <Effect> for <Type>`.
        if let Some(EffectRef::Concrete {
            name: interface_name,
            ..
        }) = &effect
        {
            // Skip the check for an unresolved handler (`Unknown` / `Error`):
            // an earlier diagnostic already covered it. Query by `TypeId` (via
            // `type_implements_trait`) so a generic-instance handler like
            // `Ctx<i32>` is recognised through its `impl<T> Log for Ctx<T>`.
            let underlying = self.tysys.type_table.borrow().get(handler_type).clone();
            let is_real_type = !matches!(underlying, ResolvedType::Unknown | ResolvedType::Error);
            // A bare type parameter (`with E => h do` where `h: &H`, `H: E`)
            // cannot be installed: dispatch synthesis runs before
            // monomorphization and has no concrete impl to route the effect
            // operations to. `type_implements_trait` would accept it (its bound
            // names the effect), so reject it explicitly rather than let it
            // reach synthesis, which would panic.
            let is_type_param = matches!(
                underlying,
                ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
            );
            if is_real_type
                && (is_type_param
                    || !effect_decl.is_some_and(|trait_| {
                        self.tysys.type_implements_trait(
                            &self.annotate_ctx,
                            &self.type_lookup(),
                            handler_type,
                            trait_,
                        )
                    }))
            {
                let type_name = self.tysys.type_table.borrow().type_name(handler_type);
                let _ = self.emit(TypeError::HandlerEffectNotImplemented {
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

        // Record this explicit binding so
        // reify_with_handler reads the single-effect entry without
        // re-resolving the effect reference.
        if let Some(EffectRef::Concrete {
            name: eff_name,
            module_source: eff_module,
        }) = &effect
        {
            // Name the effect by its declaration, so `use { Random as Rng }`
            // records the entry a plain import would.
            let declared = effect_decl
                .map(|def| crate::name::FqTraitName::declared(self.tysys.resolutions.defs(), def));
            let name = declared
                .as_ref()
                .map(|fq| fq.base_name().to_string())
                .unwrap_or_else(|| eff_name.clone());
            let module_source = declared
                .as_ref()
                .and_then(|fq| fq.module())
                .cloned()
                .unwrap_or_else(|| eff_module.clone());
            self.record_handler_binding_facts(
                binding.id,
                super::sem::types::HandlerBindingFacts {
                    effects: vec![super::sem::types::HandlerEffectEntry {
                        impl_def: effect_decl.and_then(|trait_| {
                            self.effect_impl_block(handler_type, trait_, &trait_type_args)
                        }),
                        name,
                        module_source,
                        trait_type_args,
                    }],
                    bundle_group: None,
                    handler_type,
                },
            );
        }
    }

    /// Annotate `handler_expr` — the bundled form.
    ///
    /// Walks every `impl <Effect> for <T>` block where `T` is the handler
    /// value's underlying type, records the per-effect enumeration + shared
    /// `bundle_group`, and emits diagnostics for unsupported handler types
    /// or empty-impl cases.
    fn resolve_bundled_handler_binding(
        &mut self,
        binding: &ast::EffectHandlerBinding,
        ctx: &mut FunctionContext,
        next_bundle_group: &mut u32,
    ) {
        let handler = self.resolve_expr(&binding.handler, ctx, None);
        let handler_type = self.handler_underlying_type(handler);

        let resolved = self.tysys.type_table.borrow().get(handler_type).clone();
        match resolved {
            // Already-diagnosed shapes: keep quiet so we don't pile a
            // bundled-specific error on top.
            ResolvedType::Unknown | ResolvedType::Error => return,
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
            // emit a proper diagnostic instead of panicking.
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
                return;
            }
        }

        let type_name = self.handler_impl_target_name(handler_type);
        let effects = self.collect_effect_impls_for_type(handler_type);

        if effects.is_empty() {
            let _ = self
                .logger
                .error(TypeError::BundledHandlerImplementsNoEffect {
                    type_name,
                    span: binding.span,
                });
            return;
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

        // Record the bundled binding's effect
        // enumeration so reify_with_handler reproduces the same list
        // without re-running collect_effect_impls_for_type.
        self.record_handler_binding_facts(
            binding.id,
            super::sem::types::HandlerBindingFacts {
                effects,
                bundle_group: Some(bundle_group),
                handler_type,
            },
        );
    }

    /// One entry per in-scope `impl` whose trait resolves to an effect or a
    /// resource — both are installable, so a bundled `with h do` expands to one
    /// binding each. Dedup by `(module, name, type args)` keeps `Stream<u8>`
    /// and `Stream<i32>` separate.
    fn collect_effect_impls_for_type(
        &mut self,
        handler_type: TypeId,
    ) -> Vec<super::sem::types::HandlerEffectEntry> {
        let mut out: Vec<super::sem::types::HandlerEffectEntry> = Vec::new();
        let mut seen: crate::hashmap::IndexSet<(ModuleSource, String, Vec<TypeId>)> =
            crate::hashmap::IndexSet::default();

        // Collect the impl `trait_type` ASTs first so subsequent
        // `resolve_type` calls (which need `&mut self`) don't fight the
        // borrow on `trait_env.impl_headers`.
        let mut trait_types: Vec<(crate::defs::DefId, crate::defs::DefId, ast::Type)> = Vec::new();
        if let Some(entries) = self
            .tysys
            .trait_env
            .impl_index
            .get(&self.handler_impl_target(handler_type))
        {
            for key in entries {
                let Some(header) = self.tysys.trait_env.impl_headers.get(key) else {
                    continue;
                };
                // The header's own resolved reference: `impl Log for Ctx` names
                // `Log` in the module that wrote the block, which is not this
                // one and may declare a different `Log`.
                if let (Some(trait_ref), Some(trait_type)) =
                    (header.trait_ref, header.trait_type.as_ref())
                {
                    trait_types.push((*key, trait_ref, trait_type.clone()));
                }
            }
        }

        for (impl_def, trait_ref, trait_type) in &trait_types {
            // Accept either an effect or a resource declaration. The
            // dispatch synthesis pass treats both uniformly.
            if !(self.tysys.trait_env.effect_decl_index.contains(trait_ref)
                || self.tysys.trait_env.resource_decl_index.contains(trait_ref))
            {
                continue;
            }
            // Name the effect by its declaration, as the explicit form does, so
            // `use { Random as Rng }` records the entry a plain import would.
            let defs = self.tysys.resolutions.defs();
            let base_trait_name = crate::name::FqTraitName::declared(defs, *trait_ref)
                .base_name()
                .to_string();
            let decl_module = defs.module(*trait_ref).clone();
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
                out.push(super::sem::types::HandlerEffectEntry {
                    impl_def: Some(*impl_def),
                    name: base_trait_name,
                    module_source: decl_module,
                    trait_type_args: type_args,
                });
            }
        }

        out
    }

    /// The `impl <effect> for <handler>` block a `with Effect => h do` clause
    /// installs, picked by `trait_type_args` where one type implements several
    /// instantiations of one generic effect.
    fn effect_impl_block(
        &self,
        handler_type: TypeId,
        effect_decl: crate::defs::DefId,
        trait_type_args: &[TypeId],
    ) -> Option<crate::defs::DefId> {
        let keys = self
            .tysys
            .trait_env
            .impl_index
            .get(&self.handler_impl_target(handler_type))?;
        let implements = |key: &crate::defs::DefId| {
            self.tysys
                .trait_env
                .impl_headers
                .get(key)
                .is_some_and(|header| header.trait_ref == Some(effect_decl))
        };
        keys.iter()
            .find(|key| {
                implements(key)
                    && self
                        .tysys
                        .signatures
                        .impl_sig(**key)
                        .is_some_and(|sig| sig.trait_type_args == trait_type_args)
            })
            .or_else(|| keys.iter().find(|key| implements(key)))
            .copied()
    }

    /// The impl-index key for a handler type: its own declaration, not what
    /// the installing module's scope makes of the written name — the handler
    /// may be declared elsewhere, or shadowed by a same-named type here.
    fn handler_impl_target(&self, handler_type: TypeId) -> super::trait_env::ImplTargetKey {
        let name = self.handler_impl_target_name(handler_type);
        self.impl_target_of(handler_type, &crate::name::DeclName::new(name))
    }

    /// The name an `impl <effect> for <handler>` block is indexed under — the
    /// bare head, so `impl<T> Log for Ctx<T>` answers for a `Ctx<i32>` handler.
    fn handler_impl_target_name(&self, handler_type: TypeId) -> String {
        let resolved = self.tysys.type_table.borrow().get(handler_type).clone();
        match &resolved {
            ResolvedType::GenericInstance { def, .. }
            | ResolvedType::GenericResource { def, .. } => {
                self.tysys.type_table.borrow().def_name(*def).to_string()
            }
            _ => self.tysys.type_table.borrow().type_name(handler_type),
        }
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

    /// Annotate `resume value`, which yields `()` — at source level it is
    /// control flow, and dispatch synthesis later lowers it to `Return { value }`
    /// for the ordinary return-type rules to check. Returns a placeholder:
    /// missing-return analysis reads the definite exit off the AST and reify
    /// rebuilds the node, so this walk only resolves the value for its facts.
    pub(super) fn resolve_resume(
        &mut self,
        resume: &ast::ResumeExpr,
        ctx: &mut FunctionContext,
    ) -> TypeId {
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
            self.typecheck(value, ctx.return_type, resume.span);
        }

        TypeTable::UNIT
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
        ResolvedType::BuiltinArray(_) => "array".to_string(),
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
