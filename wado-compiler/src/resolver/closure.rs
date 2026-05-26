//! Closure expression resolution (mutable and immutable captures).

use crate::hashmap::IndexSet;

use crate::ast::{self};
use crate::compiler_host::CompilerHost;
use crate::tir::{
    EffectRef, ResolvedType, TirBlock, TirCapture, TirExpr, TirExprKind, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};

use super::Resolver;
use super::types::{FunctionContext, TypeError};
use crate::hashmap::IndexMap;

/// Expected function-type info extracted from an `expected_type` hint.
///
/// A closure is contextually typed against this hint: unannotated parameters
/// default to the expected positional param type, and the body is resolved
/// against the expected return type so e.g. struct-literal bodies elaborate
/// correctly. The closure's *effect* set is left as an empty list rather
/// than copied from the hint — let-statement resolution in
/// `resolver/stmt.rs` handles function-type assignability structurally, so
/// the closure expression and the let annotation can carry different
/// `effects` lists without a spurious `TypeMismatch`.
struct ExpectedFn {
    params: Vec<TypeId>,
    return_type: TypeId,
    /// Concrete effect set declared at the use site, when one is available.
    /// `None` when the expected type is unavailable or has any
    /// `EffectRef::Param` (generic-effect bound contexts) — in those cases
    /// the closure body keeps inheriting outer effects.
    declared_effects: Option<Vec<EffectRef>>,
}

impl<H: CompilerHost> Resolver<'_, H> {
    fn extract_expected_fn(&self, expected_type: Option<TypeId>) -> Option<ExpectedFn> {
        let tid = expected_type?;
        let tt = self.type_table.borrow();
        // See through newtype layers so a closure assigned to a `type Handler =
        // fn(...)` newtype still gets its parameter types inferred from the
        // underlying fn signature.
        let base_id = tt.get_ultimate_base_type(tid);
        match tt.get(base_id) {
            ResolvedType::Function {
                params,
                return_type,
                effects,
                ..
            } => {
                // Adopt the declared effect set only when the expected fn
                // type explicitly declares at least one concrete effect.
                //
                // Two cases we deliberately skip:
                //
                // * Empty effect set (`fn(...)` / `fn mut(...)` without a
                //   `with` clause). Stdlib parameter types (e.g.
                //   `Iterator::for_each`, `Array::sort_by`) declare empty
                //   effects today — the `<effect E>` polymorphism Phase 7
                //   defers is what would mark them as effect-transparent.
                //   Until that lands, treating empty as a binding constraint
                //   would reject every effectful closure passed to those
                //   methods, even though the call site is patently capable
                //   of supplying the closure's effects.
                //
                // * Any `EffectRef::Param` entry (generic `<effect E>`
                //   bounds). Param ids are opaque to the effect checker, so
                //   swapping to them would produce spurious errors.
                //
                // In both cases, the closure body inherits the enclosing
                // function's effects — matching the documented Phase 7
                // contract that "closure literals already inherit caller
                // effects". The leak from `let inner: fn() = ||{println}` is
                // a separate, deferred concern (proper fix needs body-effect
                // inference, WEP 2026-01-25 option (a)).
                let declared_effects =
                    if effects.is_empty() || effects.iter().any(EffectRef::is_param) {
                        None
                    } else {
                        Some(effects.clone())
                    };
                Some(ExpectedFn {
                    params: params.clone(),
                    return_type: *return_type,
                    declared_effects,
                })
            }
            _ => None,
        }
    }

    /// Resolve a closure parameter's type, defaulting unannotated params to
    /// the expected-type's positional param when one is available.
    fn closure_param_type(
        &mut self,
        param: &ast::ClosureParam,
        index: usize,
        expected_fn: Option<&ExpectedFn>,
    ) -> TypeId {
        if let Some(ty) = &param.ty {
            return self.resolve_type(ty);
        }
        if let Some(ef) = expected_fn
            && let Some(t) = ef.params.get(index)
        {
            return *t;
        }
        TypeTable::UNKNOWN
    }
}

impl<H: CompilerHost> Resolver<'_, H> {
    /// Reject default parameter values on closures. Parser accepts the syntax
    /// for uniform recovery, but defaults cannot survive the fn-type erasure
    /// closures undergo, so they're rejected here.
    fn reject_closure_defaults(&mut self, closure: &ast::ClosureExpr) {
        for param in &closure.params {
            if let Some(default) = &param.default {
                let _ = self.logger.error(TypeError::DefaultInClosure {
                    param: param.name.clone(),
                    span: default.span(),
                });
            }
        }
    }

    /// Resolve a closure expression. Auto-capture by reference: every outer
    /// binding that the body assigns to is captured as `&mut T`; reads alone
    /// keep the original by-value capture path. The closure type is tagged
    /// `fn mut(...)` when any capture is mutating, otherwise `fn(...)`.
    pub(super) fn resolve_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        self.reject_closure_defaults(closure);
        let expected_fn = self.extract_expected_fn(expected_type);
        let span = closure.span;

        // Step 1: Collect outer bindings the body assigns to.
        let mut assigned_names: IndexSet<String> = IndexSet::default();
        Self::collect_mutated_vars(&closure.body, &mut assigned_names);

        // Step 2: For each assigned name that resolves to an outer `mut`
        // local, materialize a `&mut T` reference in the outer context and
        // rewrite inner uses to deref that reference.
        let mut ref_stmts: Vec<TirStmt> = Vec::new();
        let mut deref_overrides: IndexMap<String, (String, TypeId)> = IndexMap::default();
        let mut any_mutating_capture = false;

        for var_name in &assigned_names {
            if let Some(local) = ctx.lookup(var_name)
                && local.is_mut
            {
                any_mutating_capture = true;
                let inner_type = local.type_id;
                let outer_index = local.index;
                let ref_type = self.type_table.borrow_mut().make_mut_ref(inner_type);
                let ref_name = format!("__ref_{var_name}");
                let ref_index = ctx.add_local(ref_name.clone(), ref_type, false, None);
                ctx.address_taken_locals.insert(outer_index);

                ref_stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: ref_name.clone(),
                        local_index: ref_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: ref_type,
                        value: TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::MutRef,
                                expr: Box::new(TirExpr::new(
                                    TirExprKind::Local {
                                        index: outer_index,
                                        name: var_name.clone(),
                                    },
                                    inner_type,
                                    span,
                                )),
                            },
                            ref_type,
                            span,
                        ),
                        skip_value_copy: false,
                    },
                    span,
                ));

                deref_overrides.insert(var_name.clone(), (ref_name, inner_type));
            }
        }

        // Step 3: Open the closure scope with the deref overrides.
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.type_table);
        closure_ctx.deref_overrides = deref_overrides;

        // Step 4: Add closure parameters.
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let type_id = self.closure_param_type(p, i, expected_fn.as_ref());
                closure_ctx.add_local(p.name.clone(), type_id, p.is_mut, Some(p.id));
                self.record_local_symbol(p.id, &p.name, p.name_span, p.is_mut, type_id);
                (p.name.clone(), type_id)
            })
            .collect();

        // Step 5: Resolve the body, forwarding the expected return type so
        // e.g. struct-literal bodies can be elaborated against it.
        let body_expected = expected_fn.as_ref().map(|ef| ef.return_type);
        let body = self.resolve_expr(&closure.body, &mut closure_ctx, body_expected);

        // Step 6: Build the capture list.
        let captures: Vec<TirCapture> = closure_ctx
            .get_captures()
            .into_iter()
            .map(|(name, _index, local)| TirCapture {
                name,
                outer_index: local.index,
                type_id: local.type_id,
                is_mut: local.is_mut,
            })
            .collect();

        // Step 7: Determine the return type.
        //
        // When the body is a Block, prefer the type of any explicit
        // `return` statement; if there is none, fall back to the block's
        // own result type. `body.type_id` (computed by `block_result_type`)
        // keeps a diverging-tail block like `{ panic("oops"); }` as
        // `Never` rather than collapsing to `Unit` — otherwise a closure
        // passed to `fn<T>(f: fn() -> T) -> T` would bind `T = ()` and
        // let the caller run past the call site.
        //
        // If the body contains an explicit `return` but does NOT always
        // exit (`if cond { return 1; }` with no `else`), the fall-through
        // path silently yields `body.type_id` while the explicit-return
        // path yields the returned type — Wasm validation would reject
        // the generated functor. Treat this as missing-return.
        let return_type = if let TirExprKind::Block(ref block) = body.kind {
            match Self::find_return_type_in_block(block) {
                Some(t) => {
                    if !Self::block_always_exits(block)
                        && body.type_id != t
                        && body.type_id != TypeTable::NEVER
                    {
                        let _ = self.logger.error(TypeError::MissingReturn {
                            return_type: self.type_table.borrow().type_name(t),
                            span: closure.span,
                        });
                    }
                    t
                }
                None if body.type_id == TypeTable::UNIT || body.type_id == TypeTable::NEVER => {
                    body.type_id
                }
                None => {
                    let _ = self.logger.error(TypeError::MissingReturn {
                        return_type: self.type_table.borrow().type_name(body.type_id),
                        span: closure.span,
                    });
                    TypeTable::UNIT
                }
            }
        } else {
            body.type_id
        };

        // Step 8: Build the closure's function type. `is_mut` is set when any
        // outer binding is mutated by the body. Effects/stores stay empty for
        // the same reason as before: function-type assignability is
        // structural in params/return and ignores effects.
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.type_table.borrow_mut().make_function_with_mut(
            any_mutating_capture,
            param_types,
            return_type,
            Vec::new(),
            Vec::new(),
        );

        let mut all_locals = closure_ctx.locals;
        let body_locals = all_locals.split_off(params.len());

        let declared_effects = expected_fn
            .as_ref()
            .and_then(|ef| ef.declared_effects.clone());

        let closure_tir = TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
                functor_id: None,
                address_taken_locals: closure_ctx.address_taken_locals,
                body_locals,
                declared_effects,
            },
            func_type,
            closure.span,
        );

        // Step 9: If we materialized `&mut` ref locals, wrap the closure
        // expression in a block that binds them first.
        if ref_stmts.is_empty() {
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
}
