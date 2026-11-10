//! Annotation pass for effect handler installation (`with E => h do { … }`);
//! see WEP 2026-04-11. It validates that each binding names a real effect
//! declaration and that the handler's stripped type has an `impl` in scope,
//! recording only `HandlerBindingFacts` for reify to rebuild the TIR nodes
//! from. Where `resume` and `return` may stand is checked in
//! `stmt.rs::resolve_return`.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{EffectRef, ResolvedType, StructDef, TypeArgMap, TypeId};

use super::Elaborator;
use super::types::{FunctionContext, TypeError};
use super::tysys::TypeSystem;
use crate::defs::DefId;
use crate::elaborator::sem::types::{HandlerBindingFacts, HandlerEffectEntry};
use crate::elaborator::solver_bridge::{InstanceBody, InstanceImpl, InstanceOrder};
use crate::elaborator::trait_env::ImplTargetKey;
use crate::hashmap;
use crate::name::{DeclName, DeclaredHead, FqTypeName, TUPLE_TYPE_NAME};

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Annotate `with E1 => h1, ... do { body }`. Walks each handler
    /// binding for fact recording + diagnostics, walks the body for
    /// fact recording, and returns a placeholder.
    ///
    /// Reify rebuilds the `WithHandler` node — its handler bindings from
    /// `HandlerBindingFacts` and its body from the AST; the missing-return
    /// analysis in `control_flow.rs` reads `with` off the AST too.
    /// Nothing needs a resolved node, so this arm records facts and projects
    /// the body's result type.
    pub(super) fn resolve_with_handler(
        &mut self,
        with_expr: &ast::WithHandlerExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        // Counter feeding `HandlerBindingFacts.bundle_group` for the
        // bundled-handler form. A bundled clause may expand into multiple
        // effect bindings that share one synthesised `$h_<bundle>` local
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
        self.resolve_block_value(&with_expr.body, &mut ctx.enter_scope(), expected_type);

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
        // body walk only resolves the body for its fact-recording side
        // effects and projects the result type. Missing-return analysis reads
        // `with` off the AST via `control_flow.rs`, so nothing
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
        let interface_name = self.get_type_name(effect_ty);
        let site = effect_ty.id();
        let effect = self.effect_named_at(site, effect_ty.span(), &interface_name);
        let effect_decl = self.tysys.resolutions.head_decl(effect_ty);

        // Both an effect and a resource are installable as handlers (see WEP
        // 2026-04-11); a polymorphic effect parameter is not.
        if let Some(EffectRef::Param { name }) = &effect {
            let _ = self.emit(TypeError::GenericEffectParamNotInstallable {
                name: name.clone(),
                span: effect_ty.span(),
            });
        }

        // Resolve the handler value expression in the outer scope.
        let handler = self.resolve_expr(&binding.handler, ctx, None);
        // A handler's `impl Effect for T` is indexed by `T`, not `&T`.
        let handler_type = self.tysys.through_ref(handler);

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

        // The block the order selects for the handler at the clause's
        // arguments is the one installed; anything else is diagnosed here,
        // since dispatch synthesis has no block to route the operations to.
        if let Some(EffectRef::Concrete(head)) = &effect {
            let underlying = self.tysys.type_table.borrow().get(handler_type).clone();
            let mut impl_def = None;
            match underlying {
                // Diagnosed earlier.
                ResolvedType::Unknown | ResolvedType::Error => {}
                // A bare type parameter (`with E => h do` where `h: &H`,
                // `H: E`) cannot be installed: dispatch synthesis runs before
                // monomorphization and has no concrete impl to route the
                // effect operations to.
                ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } => {
                    self.report_handler_not_implemented(handler_type, &interface_name, binding);
                }
                _ => match effect_decl.and_then(|trait_| {
                    self.effect_impl_order(handler_type, trait_, &trait_type_args)
                }) {
                    Some(InstanceOrder::One(InstanceImpl {
                        body: InstanceBody::Block(block),
                        ..
                    })) => impl_def = Some(block),
                    Some(InstanceOrder::Tied(tied)) => {
                        self.report_tied_impls(&tied, handler_type, binding.span);
                    }
                    Some(InstanceOrder::One(_) | InstanceOrder::Nothing) | None => {
                        self.report_handler_not_implemented(handler_type, &interface_name, binding);
                    }
                },
            }
            // Reify reads the single-effect entry without re-resolving the
            // effect reference.
            self.record_handler_binding_facts(
                binding.id,
                HandlerBindingFacts {
                    effects: vec![HandlerEffectEntry {
                        impl_def,
                        effect: head.clone(),
                        trait_type_args,
                    }],
                    bundle_group: None,
                    handler_type,
                },
            );
        }
    }

    fn report_handler_not_implemented(
        &mut self,
        handler_type: TypeId,
        interface_name: &str,
        binding: &ast::EffectHandlerBinding,
    ) {
        let type_name = self.tysys.type_table.borrow().name(handler_type);
        let _ = self.emit(TypeError::HandlerEffectNotImplemented {
            type_name,
            interface_name: interface_name.to_string(),
            span: binding.span,
        });
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
        let handler_type = self.tysys.through_ref(handler);

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
            | ResolvedType::Tuple(_)
            | ResolvedType::Primitive(_) => {}
            // Anything else can't be the target of an `impl Effect for T`
            // block. These cases are reachable from user code — the most
            // common is a `&T` where `T` is a generic parameter — so we
            // emit a proper diagnostic instead of panicking.
            ref other => {
                let type_name = self.tysys.type_table.borrow().name(handler_type);
                let type_kind = describe_resolved_type_kind(other);
                let _ = self.emit(TypeError::BundledHandlerUnsupportedHandlerType {
                    type_name,
                    type_kind,
                    span: binding.span,
                });
                return;
            }
        }

        let type_name = self.tysys.handler_impl_target_name(handler_type);
        let (effects, ties) = self.collect_effect_impls_for_type(handler_type);
        for tied in &ties {
            self.report_tied_impls(tied, handler_type, binding.span);
        }
        if !ties.is_empty() {
            return;
        }
        if effects.is_empty() {
            let _ = self.emit(TypeError::BundledHandlerImplementsNoEffect {
                type_name,
                span: binding.span,
            });
            return;
        }

        // All bindings expanded from this single bundled clause share one
        // `bundle_group` id, so the dispatch synthesis allocates a single
        // shared `$h_<bundle>` local. This is what makes value-form
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
            HandlerBindingFacts {
                effects,
                bundle_group: Some(bundle_group),
                handler_type,
            },
        );
    }

    /// One entry per effect or resource the handler type implements — both
    /// are installable, so a bundled `with h do` expands to one binding each —
    /// and each tie the order leaves among one effect's blocks. Dedup by the
    /// effect's declaration and the type args' keys keeps `Stream<u8>` and
    /// `Stream<i32>` separate, two modules' same-named effects too.
    fn collect_effect_impls_for_type(
        &self,
        handler_type: TypeId,
    ) -> (Vec<HandlerEffectEntry>, Vec<Vec<Option<DefId>>>) {
        let defs = self.tysys.resolutions.defs();
        let mut out: Vec<HandlerEffectEntry> = Vec::new();
        let mut ties: Vec<Vec<Option<DefId>>> = Vec::new();
        let mut seen: hashmap::IndexSet<(DefId, Vec<TypeId>)> = hashmap::IndexSet::default();

        let trait_env = &self.tysys.trait_env;
        let written = trait_env
            .impl_index
            .get(&self.handler_impl_target(handler_type))
            .into_iter()
            .flatten()
            .copied();
        let blankets = trait_env
            .blanket_impls
            .iter()
            .filter(|(trait_, _)| defs.kind(**trait_).is_effect())
            .flat_map(|(_, blankets)| blankets.iter().map(|blanket| blanket.def));
        for impl_def in written.chain(blankets) {
            // Everything the block says about itself it says in its own frame:
            // `impl Stream<u8> for Ctx` names `Stream` and resolves `u8` where
            // the block was written, which is not where it is installed. A
            // slot the block leaves to its own parameter takes what matching
            // its target against the handler binds: `impl<T> Stream<T> for
            // Ctx<T>` on a `Ctx<u8>` installs `Stream<u8>`, and a blanket's
            // parameter is the handler itself.
            let Some(trait_ref) = trait_env.impl_headers[&impl_def].trait_def() else {
                continue;
            };
            if !defs.kind(trait_ref).is_effect() {
                continue;
            }
            let Some(slots) = self.handler_slots(impl_def, handler_type) else {
                continue;
            };
            let type_args = self
                .tysys
                .signatures
                .impl_sig(impl_def)
                .instantiate_slots(&self.tysys.type_table, &slots)
                .trait_type_args;
            // A block answers only where the order selects it for the
            // handler: a sibling written for another instance (`Ctx<u8>`
            // beside `Ctx<i32>`) does not, nor a blanket a written impl
            // outranks.
            let selected = match self.effect_impl_order(handler_type, trait_ref, &type_args) {
                Some(InstanceOrder::One(InstanceImpl {
                    body: InstanceBody::Block(block),
                    ..
                })) => block == impl_def,
                Some(InstanceOrder::Tied(tied)) => {
                    if tied.contains(&Some(impl_def)) && !ties.contains(&tied) {
                        ties.push(tied);
                    }
                    false
                }
                Some(InstanceOrder::One(_) | InstanceOrder::Nothing) | None => false,
            };
            if !selected {
                continue;
            }
            if seen.insert((trait_ref, type_args.clone())) {
                out.push(HandlerEffectEntry {
                    impl_def: Some(impl_def),
                    effect: DeclaredHead::new(defs, trait_ref),
                    trait_type_args: type_args,
                });
            }
        }

        (out, ties)
    }

    /// Block `impl_def`'s slots bound by its target matching `handler_type`;
    /// `None` where the target names another instance (`Ctx<u8>` for a
    /// `Ctx<i32>` handler).
    fn handler_slots(&self, impl_def: DefId, handler_type: TypeId) -> Option<TypeArgMap> {
        let mut tt = self.tysys.type_table.borrow_mut();
        let target = tt.impl_target_whole(impl_def);
        let mut slots = tt.bind_type_params(&[target], &[handler_type])?;
        tt.project_impl_slots(impl_def, &mut slots);
        Some(slots)
    }

    /// What the order selects for `effect_decl<trait_type_args>` on the
    /// handler, as it does for a call: a block written for the handler, one
    /// leaving a slot the arguments fill (`impl<T> Stream<T> for Ctx<T>`
    /// installed as `with Stream<u8>`), or a blanket whose bounds the handler
    /// meets.
    fn effect_impl_order(
        &self,
        handler_type: TypeId,
        effect_decl: DefId,
        trait_type_args: &[TypeId],
    ) -> Option<InstanceOrder> {
        let tt = self.tysys.type_table.borrow();
        let args: Vec<FqTypeName> = trait_type_args
            .iter()
            .map(|&arg| tt.fq_type_name(arg))
            .collect();
        self.tysys
            .solver()
            .instance_order(&tt, handler_type, effect_decl, &args)
    }

    /// The impl-index key for a handler type: its own declaration, not what
    /// the installing module's scope makes of the written name — the handler
    /// may be declared elsewhere, or shadowed by a same-named type here.
    fn handler_impl_target(&self, handler_type: TypeId) -> ImplTargetKey {
        let name = self.tysys.handler_impl_target_name(handler_type);
        self.impl_target_of(handler_type, &DeclName::new(name))
    }
}

impl TypeSystem {
    /// The name an `impl <effect> for <handler>` block is indexed under — the
    /// bare head, so `impl<T> Log for Ctx<T>` answers for a `Ctx<i32>` handler.
    fn handler_impl_target_name(&self, handler_type: TypeId) -> String {
        let resolved = self.type_table.borrow().get(handler_type).clone();
        match &resolved {
            ResolvedType::Variant { type_args, .. } if type_args.is_empty() => {
                self.type_table.borrow().name(handler_type)
            }
            ResolvedType::Struct {
                def: StructDef::Decl(def),
                type_args,
            } if !type_args.is_empty() => self.type_table.borrow().decl_name(*def).to_string(),
            ResolvedType::Variant { def, .. } | ResolvedType::Resource { def, .. } => {
                self.type_table.borrow().decl_name(*def).to_string()
            }
            ResolvedType::Tuple(_) => TUPLE_TYPE_NAME.to_string(),
            _ => self.type_table.borrow().name(handler_type),
        }
    }
}

/// Short label for a [`ResolvedType`] variant, used in diagnostic
/// messages emitted by the bundled-handler elaborator. Only covers the
/// shapes the elaborator rejects; the supported shapes return their own
/// names via `TypeTable::name`.
fn describe_resolved_type_kind(ty: &ResolvedType) -> String {
    match ty {
        ResolvedType::TypeParam { .. } => "type parameter".to_string(),
        ResolvedType::AssocTypeProjection { .. } => "associated type projection".to_string(),
        ResolvedType::Function { .. } => "function type".to_string(),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => "nested reference type".to_string(),
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
