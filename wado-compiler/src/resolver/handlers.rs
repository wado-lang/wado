//! Resolver lowering for effect handler installation (`with E = h do { ... }`)
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
//! Bundled handlers (`with &mut h do`) are diagnosed as not-yet-implemented;
//! they require enumerating every effect the handler's type implements and
//! are deferred until the dispatch synthesis can support that.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{
    EffectRef, ResolvedType, TirExpr, TirExprKind, TirHandlerBinding, TypeId, TypeTable,
};

use super::Resolver;
use super::types::{FunctionContext, TypeError};

impl<H: CompilerHost> Resolver<'_, H> {
    /// Resolve `with E1 = h1, ... do { body }`.
    pub(super) fn resolve_with_handler(
        &mut self,
        with_expr: &ast::WithHandlerExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let mut bindings = Vec::with_capacity(with_expr.handlers.len());

        for binding in &with_expr.handlers {
            let resolved = self.resolve_handler_binding(binding, ctx);
            bindings.push(resolved);
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
    /// Validation order:
    /// 1. Reject the bundled (`effect: None`) form — not yet implemented.
    /// 2. Resolve the effect name to an `EffectRef::Concrete` and verify it
    ///    actually points at an `effect` declaration (not a trait or unknown
    ///    name).
    /// 3. Resolve the handler expression and strip a leading reference layer
    ///    so we can locate the underlying struct type for `impl E for T`.
    /// 4. Verify that struct type has an `impl Effect for Type` block.
    fn resolve_handler_binding(
        &mut self,
        binding: &ast::EffectHandlerBinding,
        ctx: &mut FunctionContext,
    ) -> TirHandlerBinding {
        // Bundled-handler form is reserved for the multi-effect case
        // (`with &mut h do`) and requires enumerating effects from the
        // handler type. Defer to the dispatch-synthesis phase work.
        let effect_ty = match &binding.effect {
            Some(ty) => ty,
            None => {
                let _ = self
                    .logger
                    .error(TypeError::BundledHandlerNotSupported { span: binding.span });
                let handler = self.resolve_expr(&binding.handler, ctx, None);
                let handler_type = self.handler_underlying_type(handler.type_id);
                return TirHandlerBinding {
                    effect: None,
                    handler,
                    handler_type,
                    span: binding.span,
                };
            }
        };

        // Resolve the effect name. Use `resolve_effects` so that LSP
        // jump-to-def edges are recorded just like in `with E1, E2`
        // function signatures.
        let effect_name = self.get_type_name(effect_ty);
        let effect_ids = effect_ty
            .id()
            .map(|id| vec![(id, effect_ty.span())])
            .unwrap_or_default();
        let mut resolved_effects = self.resolve_effects(&[effect_name.clone()], &effect_ids);
        let effect = resolved_effects.pop();

        // The name must point at an actual effect declaration, not a
        // regular trait or arbitrary identifier. Param effects (generic
        // `<effect E>`) are also rejected for installation: you cannot
        // install a handler for a polymorphic effect parameter.
        if let Some(eff) = &effect {
            match eff {
                EffectRef::Concrete {
                    name,
                    module_source,
                } => {
                    let is_known_effect = self
                        .trait_env
                        .effect_decl_index
                        .get(name)
                        .is_some_and(|(decl_module, _)| decl_module == module_source);
                    if !is_known_effect {
                        let _ = self.logger.error(TypeError::NotAnEffect {
                            name: name.clone(),
                            span: effect_ty.span(),
                        });
                    }
                }
                EffectRef::Param { name } => {
                    let _ = self.logger.error(TypeError::NotAnEffect {
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
            name: effect_name, ..
        }) = &effect
        {
            let type_name = self.type_table.borrow().type_name(handler_type);
            // Skip the impl-presence check when the handler type is unresolved
            // (e.g. `Unknown` / `Error`), because earlier diagnostics already
            // reported the handler-side problem.
            let is_real_type = !matches!(
                self.type_table.borrow().get(handler_type),
                ResolvedType::Unknown | ResolvedType::Error
            );
            if is_real_type && !self.find_trait_impl_for_type(&type_name, effect_name) {
                let _ = self.logger.error(TypeError::HandlerEffectNotImplemented {
                    type_name,
                    effect_name: effect_name.clone(),
                    span: binding.span,
                });
            }
        }

        TirHandlerBinding {
            effect,
            handler,
            handler_type,
            span: binding.span,
        }
    }

    /// Strip a single leading `&` / `&mut` layer to reach the type that the
    /// handler value points at. The handler's `impl Effect for T` block is
    /// indexed by `T`, not `&T`.
    fn handler_underlying_type(&self, type_id: TypeId) -> TypeId {
        match self.type_table.borrow().get(type_id) {
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
            self.typecheck(value.type_id, ctx.return_type, resume.span);
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
