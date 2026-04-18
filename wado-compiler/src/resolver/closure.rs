//! Closure expression resolution (mutable and immutable captures).

use crate::hashmap::IndexSet;

use crate::ast::{self};
use crate::compiler_host::CompilerHost;
use crate::tir::{
    TirBlock, TirCapture, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::types::{FunctionContext, TypeError};
use crate::hashmap::IndexMap;

impl<H: CompilerHost> Resolver<'_, H> {
    pub(super) fn resolve_mutable_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirExpr {
        // Step 1: Find all directly-assigned outer mutable variables
        let mut assigned_names: IndexSet<String> = IndexSet::default();
        Self::collect_mutated_vars(&closure.body, &mut assigned_names);

        // Step 2: For each assigned name that is an outer mutable variable,
        // create a `&mut T` reference in the outer context
        let mut ref_stmts: Vec<TirStmt> = Vec::new();
        let mut deref_overrides: IndexMap<String, (String, TypeId)> = IndexMap::default();

        for var_name in &assigned_names {
            if let Some(local) = ctx.lookup(var_name)
                && local.is_mut
            {
                let inner_type = local.type_id;
                let outer_index = local.index;
                let ref_type = self.type_table.borrow_mut().make_mut_ref(inner_type);
                let ref_name = format!("__ref_{var_name}");
                let ref_index = ctx.add_local(ref_name.clone(), ref_type, false, None);
                ctx.address_taken_locals.insert(outer_index);

                // Emit: let __ref_name = &mut var_name
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

        // Step 3: Create closure context with deref overrides
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.type_table);
        closure_ctx.deref_overrides = deref_overrides;

        // Step 4: Add closure parameters
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .map(|p| {
                let type_id =
                    p.ty.as_ref()
                        .map(|t| self.resolve_type(t))
                        .unwrap_or(TypeTable::UNKNOWN);
                closure_ctx.add_local(p.name.clone(), type_id, p.is_mut, Some(p.id));
                self.record_local_symbol(p.id, &p.name, p.name_span, p.is_mut);
                (p.name.clone(), type_id)
            })
            .collect();

        // Step 5: Resolve body with modified context
        let body = self.resolve_expr(&closure.body, &mut closure_ctx, None);

        // Step 6: Build capture list
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

        // Step 7: Determine return type
        // For block bodies, only explicit `return` counts; expression bodies use their type
        let return_type = if let TirExprKind::Block(ref block) = body.kind {
            if let Some(t) = Self::find_return_type_in_block(block) {
                t
            } else {
                // Error if block has a trailing non-unit expression without `return`
                if body.type_id != TypeTable::UNIT && body.type_id != TypeTable::NEVER {
                    let _ = self.logger.error(TypeError::MissingReturn {
                        return_type: self.type_table.borrow().type_name(body.type_id),
                        span: closure.span,
                    });
                }
                TypeTable::UNIT
            }
        } else {
            body.type_id
        };

        // Step 8: Create function type
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.type_table.borrow_mut().make_function(
            param_types,
            return_type,
            Vec::new(),
            Vec::new(),
        );

        let closure_tir = TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
                functor_id: None,
                source_text: closure.source_text.clone(),
            },
            func_type,
            closure.span,
        );

        // Step 9: Wrap in a block if we injected ref statements
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

    /// Resolve a closure
    pub(super) fn resolve_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Create a closure context with access to outer scope for capture detection
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.type_table);

        // Add closure parameters
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .map(|p| {
                let type_id =
                    p.ty.as_ref()
                        .map(|t| self.resolve_type(t))
                        .unwrap_or(TypeTable::UNKNOWN);
                closure_ctx.add_local(p.name.clone(), type_id, p.is_mut, Some(p.id));
                self.record_local_symbol(p.id, &p.name, p.name_span, p.is_mut);
                (p.name.clone(), type_id)
            })
            .collect();

        // Resolve body - this will detect captured variables
        let body = self.resolve_expr(&closure.body, &mut closure_ctx, None);

        // Build capture list from detected captures
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

        // Determine return type:
        // - For block bodies, only explicit `return` counts
        // - For expression bodies, use the expression's type
        let return_type = if let TirExprKind::Block(ref block) = body.kind {
            if let Some(t) = Self::find_return_type_in_block(block) {
                t
            } else {
                // Error if block has a trailing non-unit expression without `return`
                if body.type_id != TypeTable::UNIT && body.type_id != TypeTable::NEVER {
                    let _ = self.logger.error(TypeError::MissingReturn {
                        return_type: self.type_table.borrow().type_name(body.type_id),
                        span: closure.span,
                    });
                }
                TypeTable::UNIT
            }
        } else {
            body.type_id
        };

        // Create function type
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.type_table.borrow_mut().make_function(
            param_types,
            return_type,
            Vec::new(),
            Vec::new(),
        );

        TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
                functor_id: None, // Assigned during lowering
                source_text: closure.source_text.clone(),
            },
            func_type,
            closure.span,
        )
    }
}
