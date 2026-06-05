//! Annotation pass for closure expressions.
//!
//! The body walk still allocates the synthetic `__ref_<var>` locals,
//! address-takes their outer bindings, walks the body so its
//! `ModuleSemantics` lands, projects the closure's `fn(...)` / `fn mut(...)`
//! type (so the caller's typecheck sees the right `expression_types`), and
//! records [`super::sem::types::ClosureCaptureInfo`]. Reify rebuilds the
//! `let __ref_* = &mut <var>;` materialisation, the
//! `TirExprKind::Closure { params, body, captures, … }`, and the optional
//! enclosing `Block` wrapper from the AST + that record.

use crate::hashmap::IndexSet;

use crate::ast::{self};
use crate::compiler_host::CompilerHost;
use crate::tir::{ResolvedType, TirStmt, TypeId, TypeTable};

use super::Elaborator;
use super::types::{FunctionContext, TypeError};
use crate::hashmap::IndexMap;

/// Expected function-type info extracted from an `expected_type` hint.
struct ExpectedFn {
    params: Vec<TypeId>,
    return_type: TypeId,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    fn extract_expected_fn(&self, expected_type: Option<TypeId>) -> Option<ExpectedFn> {
        let tid = expected_type?;
        let tt = self.tysys.type_table.borrow();
        // See through newtype layers so a closure assigned to a `type Handler =
        // fn(...)` newtype still gets its parameter types inferred from the
        // underlying fn signature.
        let base_id = tt.get_ultimate_base_type(tid);
        match tt.get(base_id) {
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => Some(ExpectedFn {
                params: params.clone(),
                return_type: *return_type,
            }),
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

impl<H: CompilerHost> Elaborator<'_, H> {
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

    pub(super) fn resolve_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        self.reject_closure_defaults(closure);
        let expected_fn = self.extract_expected_fn(expected_type);

        // Collect outer bindings the body assigns to.
        let mut assigned_names: IndexSet<String> = IndexSet::default();
        Self::collect_mutated_vars(&closure.body, &mut assigned_names);

        // For each assigned name that resolves to an outer `mut` local,
        // record a `MutCapture` (so reify replays the `__ref_<var>`
        // materialisation in the same order) and mark the outer local
        // address-taken. The `__ref_<var>` local slot is also reserved on
        // the outer `ctx` so any subsequent local-index accounting in the
        // parent function stays consistent with what reify will produce.
        let mut deref_overrides: IndexMap<String, (String, TypeId)> = IndexMap::default();
        let mut mut_captures: Vec<super::sem::types::MutCapture> = Vec::new();
        let mut any_mutating_capture = false;

        for var_name in &assigned_names {
            if let Some(local) = ctx.lookup(var_name)
                && local.is_mut
            {
                any_mutating_capture = true;
                let inner_type = local.type_id;
                let outer_index = local.index;
                let ref_type = self.tysys.type_table.borrow_mut().make_mut_ref(inner_type);
                let ref_name = format!("__ref_{var_name}");
                let _ref_index = ctx.add_local(ref_name.clone(), ref_type, false, None);
                ctx.address_taken_locals.insert(outer_index);

                mut_captures.push(super::sem::types::MutCapture {
                    var_name: var_name.clone(),
                    ref_name: ref_name.clone(),
                    inner_type,
                    ref_type,
                    outer_index,
                });
                deref_overrides.insert(var_name.clone(), (ref_name, inner_type));
            }
        }

        // Open the closure scope with the deref overrides and walk params
        // + body so their facts land on `ModuleSemantics`.
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.tysys.type_table);
        closure_ctx.deref_overrides = deref_overrides;

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

        let body_expected = expected_fn.as_ref().map(|ef| ef.return_type);
        let body_type = self.resolve_expr(&closure.body, &mut closure_ctx, body_expected);

        // Build the recorded capture list from the closure scope's captures.
        let recorded_captures: Vec<super::sem::types::CaptureEntry> = closure_ctx
            .get_captures()
            .into_iter()
            .map(|(name, _index, local)| super::sem::types::CaptureEntry {
                name,
                outer_index: local.index,
                type_id: local.type_id,
                is_mut: local.is_mut,
            })
            .collect();

        // Stage 5 (Gap 4 of WEP 2026-05-26): the only signal reify needs
        // for the closure's capture analysis.
        self.record_closure_captures(
            closure.id,
            super::sem::types::ClosureCaptureInfo {
                mut_captures,
                captures: recorded_captures,
                is_mutating: any_mutating_capture,
            },
        );

        // Diagnose missing returns / type mismatches on block-bodied
        // closures via the AST-walker. Mirrors the pre-7-B return-type
        // analysis (which gated this on `body.kind == TirExprKind::Block`)
        // so an explicit `return` inside a partial branch reports the
        // same error, without depending on the combined walk's body
        // TIR. Single-expression closure bodies (e.g.
        // `|c: char| c.to_ascii_uppercase()`) take the body's type as
        // the return type directly — no missing-return check applies.
        let return_type = if let ast::Expr::Block(ref block) = closure.body {
            match self.ast_find_return_type_in_block(block) {
                Some(t) => {
                    if !self.ast_block_always_exits(block)
                        && body_type != t
                        && body_type != TypeTable::NEVER
                    {
                        let _ = self.logger.error(TypeError::MissingReturn {
                            return_type: self.tysys.type_table.borrow().type_name(t),
                            span: closure.span,
                        });
                    }
                    t
                }
                None if body_type == TypeTable::UNIT || body_type == TypeTable::NEVER => body_type,
                None => {
                    let _ = self.logger.error(TypeError::MissingReturn {
                        return_type: self.tysys.type_table.borrow().type_name(body_type),
                        span: closure.span,
                    });
                    TypeTable::UNIT
                }
            }
        } else {
            body_type
        };

        // Project the closure's `fn(...)` / `fn mut(...)` type so the
        // caller's typecheck context (`expression_types[closure.id]`, etc.)
        // sees it. Reify rebuilds an equivalent type from its own facts.
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.tysys.type_table.borrow_mut().make_function_with_mut(
            any_mutating_capture,
            param_types,
            return_type,
            Vec::new(),
            Vec::new(),
        );

        // Placeholder — reify is the sole producer of the closure's TIR shape.
        let _ = (closure_ctx, body_type);
        let _: Vec<TirStmt> = Vec::new();
        func_type
    }
}
