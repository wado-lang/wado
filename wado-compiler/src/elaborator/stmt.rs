//! Statement resolution (let, return, if, loop, break, continue, etc.).

use crate::ast::{
    self, AstId, Block, BreakStmt, Condition, ConditionElement, ContinueStmt, Expr, ExprStmt,
    ForOfStmt, ForStmt, IfStmt, LetStmt, Literal, LoopStmt, Pattern, ReturnStmt, Stmt,
    TaskReturnStmt, Type, WhileStmt,
};
use crate::compiler_host::CompilerHost;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirLiteralPattern, TirMatchArm,
    TirPattern, TirStmt, TirStmtKind, TirStructField, TirStructPatternField, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::typecheck::{TypeCheckResult, check_assignable};
use super::types::{FunctionContext, TypeError};
use super::util;

/// Tracks the reference binding mode for match ergonomics.
/// When matching a reference-typed scrutinee, bindings inherit the reference kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefBinding {
    None,
    Ref,
    MutRef,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn resolve_block(
        &mut self,
        block: &Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirBlock {
        ctx.enter_scope();
        let len = block.stmts.len();
        let mut stmts = Vec::new();
        for (i, s) in block.stmts.iter().enumerate() {
            // Propagate expected type to the last expression/statement for coercion
            if expected_type.is_some() && i == len - 1 {
                if let Stmt::Expr(expr_stmt) = s {
                    let expr = self.resolve_expr(&expr_stmt.expr, ctx, expected_type);
                    stmts.push(TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span));
                    continue;
                }
                if let Stmt::If(if_stmt) = s {
                    stmts.extend(self.resolve_if_stmt_with_expected(if_stmt, ctx, expected_type));
                    continue;
                }
                if let Stmt::Match(match_expr) = s {
                    let tir = self.resolve_match_expr(match_expr, ctx, expected_type);
                    stmts.push(TirStmt::new(TirStmtKind::Expr(tir), match_expr.span));
                    continue;
                }
                if let Stmt::LabeledBlock(labeled_block) = s {
                    stmts.push(self.resolve_labeled_block_with_expected(
                        labeled_block,
                        ctx,
                        expected_type,
                    ));
                    continue;
                }
            }
            stmts.extend(self.resolve_stmt(s, ctx));
        }
        ctx.exit_scope();
        TirBlock::new(stmts, block.span)
    }

    /// Resolve a statement (may return multiple statements for desugared constructs)
    pub(super) fn resolve_stmt(&mut self, stmt: &Stmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        match stmt {
            Stmt::Let(let_stmt) => vec![self.resolve_let(let_stmt, ctx)],
            Stmt::Expr(expr_stmt) => vec![self.resolve_expr_stmt(expr_stmt, ctx)],
            Stmt::Return(ret_stmt) => vec![self.resolve_return(ret_stmt, ctx)],
            Stmt::TaskReturn(tr_stmt) => vec![self.resolve_task_return(tr_stmt, ctx)],
            Stmt::If(if_stmt) => self.resolve_if_stmt(if_stmt, ctx),
            Stmt::While(while_stmt) => self.resolve_while(while_stmt, ctx),
            Stmt::For(for_stmt) => self.resolve_for(for_stmt, ctx),
            Stmt::ForOf(for_of) => self.resolve_for_of(for_of, ctx),
            Stmt::Loop(loop_stmt) => vec![self.resolve_loop(loop_stmt, ctx)],
            Stmt::Match(match_expr) => {
                // A `match` in statement position discards its result, so
                // pin the expected type to `Unit`. With `expected_type =
                // Some(Unit)`, `resolve_match_expr` sets the match's
                // overall `type_id` to `Unit` regardless of what the arms
                // produce. The WIR builder then sees `has_result = false`
                // in `translate_match` and wraps each non-unit arm body in
                // `WirInstr::Drop`, so arms whose blocks evaluate to
                // different non-unit types (a separator `;` is not a
                // discard marker in Wado, so `{ helper(); }` evaluates to
                // `helper()`'s return type) do not leave a stray value on
                // the Wasm stack at the join point.
                let tir = self.resolve_match_expr(match_expr, ctx, Some(TypeTable::UNIT));
                // `resolve_match_expr` does not go through the
                // `resolve_expr` wrapper, so the per-AstId expression
                // type would be missing for stmt-position matches.
                // Record it explicitly here so LSP hover on the `match`
                // keyword and Stage 5 reify both see the resolved type
                // (Unit at stmt position).
                self.record_expression_type(match_expr.id, tir.type_id);
                vec![TirStmt::new(TirStmtKind::Expr(tir), match_expr.span)]
            }
            Stmt::Break(break_stmt) => vec![self.resolve_break(break_stmt, ctx)],
            Stmt::Continue(continue_stmt) => vec![self.resolve_continue(continue_stmt, ctx)],
            Stmt::Assert(a) => self.desugar_assert(a, ctx),
            Stmt::LabeledBlock(labeled_block) => {
                vec![self.resolve_labeled_block(labeled_block, ctx)]
            }
            // Parser error-recovery placeholder: the syntax error was already
            // reported, so emit nothing.
            Stmt::Error(_) => Vec::new(),
        }
    }

    /// Resolve a labeled block statement
    pub(super) fn resolve_labeled_block(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        self.resolve_labeled_block_with_expected(labeled_block, ctx, None)
    }

    fn resolve_labeled_block_with_expected(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirStmt {
        ctx.active_labels.push(labeled_block.label.clone());
        // resolve_block already handles scope entry/exit
        let block = self.resolve_block(&labeled_block.block, ctx, expected_type);
        ctx.active_labels.pop();

        TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: labeled_block.label.clone(),
                block,
            },
            labeled_block.span,
        )
    }

    /// Resolve a let statement
    pub(super) fn resolve_let(&mut self, let_stmt: &LetStmt, ctx: &mut FunctionContext) -> TirStmt {
        // Handle uninitialized declaration: `let x: T;` (no initializer)
        if let_stmt.value.is_none() {
            return self.resolve_uninit_let(let_stmt, ctx);
        }

        // From here on `value` is guaranteed to be Some.
        let ast_value = let_stmt.value.as_ref().unwrap();

        // Check for tuple literal to array coercion when type annotation is present
        let (value, type_id) = if let Some(annotated_type) = &let_stmt.ty {
            let target_type = self.resolve_type(annotated_type);
            // Publish the resolved whole-pattern annotation so reify reads it
            // instead of re-running `resolve_type` against the AST.
            let key = self.ann_key(let_stmt.id);
            self.sem.types.let_annotated_types.insert(key, target_type);

            // Special case: tuple literal with Tuple type annotation
            if let ast::Expr::TupleLiteral(tuple_lit) = ast_value {
                {
                    let tuple_elems = self.tysys.type_table.borrow().as_tuple(target_type);
                    if let Some(expected_elem_types) = tuple_elems {
                        let elements: Vec<TirExpr> = tuple_lit
                            .elements
                            .iter()
                            .enumerate()
                            .map(|(i, elem)| {
                                let expected = expected_elem_types.get(i).copied();
                                let resolved = self.resolve_expr(elem, ctx, expected);
                                if let Some(expected_type) = expected {
                                    self.typecheck(resolved.type_id, expected_type, elem.span());
                                }
                                resolved
                            })
                            .collect();

                        // Also check length mismatch
                        if tuple_lit.elements.len() != expected_elem_types.len() {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                expected: format!(
                                    "tuple with {} elements",
                                    expected_elem_types.len()
                                ),
                                found: format!("tuple with {} elements", tuple_lit.elements.len()),
                                span: ast_value.span(),
                            });
                        }

                        let value = TirExpr::new(
                            TirExprKind::TupleLiteral { elements },
                            target_type,
                            ast_value.span(),
                        );
                        (value, target_type)
                    } else {
                        let value = self.resolve_expr(ast_value, ctx, Some(target_type));
                        (value, target_type)
                    }
                }
            } else if let ast::Expr::StructLiteral(struct_lit) = ast_value {
                // Handle implicit struct literal: let p: Point = { x: 1, y: 2 }
                if struct_lit.name.is_none() {
                    // Check if target type is a struct
                    let target_resolved = self.tysys.type_table.borrow().get(target_type).clone();
                    if let ResolvedType::Struct {
                        name,
                        module_source,
                        ..
                    } = target_resolved
                    {
                        let struct_type = target_type;

                        let struct_field_types: Vec<(String, TypeId)> = self
                            .lookup_struct_fields(&name)
                            .map(|info| {
                                info.fields
                                    .iter()
                                    .map(|(n, t, _)| (n.clone(), *t))
                                    .collect()
                            })
                            .unwrap_or_default();

                        // Check field visibility for cross-module struct literal
                        if module_source != self.current_module_source
                            && let Some(struct_info) = self.lookup_struct_fields(&name)
                        {
                            for (fname, _, is_pub) in &struct_info.fields {
                                if !is_pub && struct_lit.fields.iter().any(|f| f.name == *fname) {
                                    let _ = self.logger.error(TypeError::PrivateFieldAccess {
                                        struct_name: name.clone(),
                                        field_name: fname.clone(),
                                        span: struct_lit.span,
                                    });
                                }
                            }
                        }

                        let fields: Vec<TirStructField> = struct_lit
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(index, field)| {
                                // Check field name exists in struct definition
                                if !struct_field_types.iter().any(|(n, _)| n == &field.name)
                                    && !struct_field_types.is_empty()
                                {
                                    let _ = self.logger.error(TypeError::ExtraField {
                                        struct_name: name.clone(),
                                        field_name: field.name.clone(),
                                        span: field.span,
                                    });
                                }
                                let expected_field_type = struct_field_types
                                    .iter()
                                    .find(|(n, _)| n == &field.name)
                                    .map(|(_, type_id)| *type_id);
                                let value =
                                    self.resolve_expr(&field.value, ctx, expected_field_type);
                                TirStructField {
                                    name: field.name.clone(),
                                    value,
                                    field_index: index as u32,
                                }
                            })
                            .collect();

                        // Record the implicit struct literal's mangled name
                        // (the target struct's name as the elaborator picks
                        // it up) so reify reads it from
                        // `GenericInstantiation` instead of taking the
                        // anon-literal path that expects a synthesised
                        // `__anon_{…}` name.
                        self.record_generic_instantiation_with_mangle(
                            struct_lit.id,
                            vec![],
                            struct_type,
                            Some(name.clone()),
                        );
                        let value = TirExpr::new(
                            TirExprKind::StructLiteral {
                                struct_type,
                                struct_name: name,
                                fields,
                            },
                            struct_type,
                            struct_lit.span,
                        );
                        (value, target_type)
                    } else if let Some(coerced) =
                        self.try_coerce_struct_to_map(ast_value, ctx, target_type)
                    {
                        (coerced, target_type)
                    } else {
                        // Target type does not implement KeyValueLiteral
                        let type_name = self.tysys.type_table.borrow().type_name(target_type);
                        let _ = self.logger.error(TypeError::MissingTraitImpl {
                            type_name,
                            trait_name: "KeyValueLiteral".to_string(),
                            span: struct_lit.span,
                        });
                        let value = self.resolve_expr(ast_value, ctx, None);
                        (value, target_type)
                    }
                } else {
                    // Named struct literal - resolve normally
                    let value = self.resolve_expr(ast_value, ctx, Some(target_type));
                    (value, target_type)
                }
            } else {
                // Use expected type for numeric literal coercion
                let value = self.resolve_expr(ast_value, ctx, Some(target_type));
                (value, target_type)
            }
        } else {
            let value = self.resolve_expr(ast_value, ctx, None);
            (value.clone(), value.type_id)
        };

        // Type check: if type annotation is present, verify value type matches.
        // Uses direct comparison instead of typecheck() because we need to catch
        // type-param-to-concrete mismatches (e.g., `let n: i32 = x` where x: T)
        // at definition time. check_assignable defers all type param cases because
        // trait impls legitimately use TypeParam-vs-concrete (monomorphized later).
        if let_stmt.ty.is_some()
            && value.type_id != type_id
            && value.type_id != TypeTable::UNKNOWN
            && value.type_id != TypeTable::NEVER
        {
            // Allow null (Option<unknown>) to be assigned to Option<T>
            let is_null_to_option = {
                let type_table = self.tysys.type_table.borrow();
                type_table
                    .as_option(value.type_id)
                    .is_some_and(|inner| inner == TypeTable::UNKNOWN)
                    && type_table.as_option(type_id).is_some()
            };
            // Function-type compatibility is structural over params and return,
            // ignoring effects (see `typecheck::check_assignable` rule 7). This
            // lets `let c: fn() with Stdout = || { ... }` accept a closure with
            // a synthesized `fn() with []` type without a spurious mismatch,
            // while still rejecting genuine signature mismatches such as a
            // closure with the wrong arity or parameter types. `check_assignable`
            // returns `Deferred` whenever either side contains a type param
            // (rule 3), so for generic signatures like `fn(T) -> T` we fall back
            // to a direct `TypeId` comparison of params and return — that
            // accepts identical generic shapes that differ only in effects
            // without admitting any type-param-to-concrete mismatches.
            let is_compatible_fn_type = {
                let type_table = self.tysys.type_table.borrow();
                if let (
                    ResolvedType::Function {
                        params: actual_params,
                        return_type: actual_return,
                        ..
                    },
                    ResolvedType::Function {
                        params: expected_params,
                        return_type: expected_return,
                        ..
                    },
                ) = (type_table.get(value.type_id), type_table.get(type_id))
                {
                    match check_assignable(value.type_id, type_id, &type_table) {
                        TypeCheckResult::Compatible => true,
                        TypeCheckResult::Deferred => {
                            actual_params.len() == expected_params.len()
                                && actual_params
                                    .iter()
                                    .zip(expected_params.iter())
                                    .all(|(a, e)| a == e)
                                && actual_return == expected_return
                        }
                        TypeCheckResult::Incompatible => false,
                    }
                } else {
                    false
                }
            };
            if !is_null_to_option && !is_compatible_fn_type {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: self.tysys.type_table.borrow().type_name(type_id),
                    found: self.tysys.type_table.borrow().type_name(value.type_id),
                    span: ast_value.span(),
                });
            }
        }

        // Handle different pattern types
        match &let_stmt.pattern {
            ast::Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | ast::Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                let is_mut =
                    let_stmt.is_mut || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                let local_index = ctx.add_local(name.clone(), type_id, is_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, is_mut, type_id);
                {
                    let mut closure_candidate = ast_value;
                    while let ast::Expr::Unary(u) = closure_candidate {
                        closure_candidate = &u.expr;
                    }
                    if let ast::Expr::Closure(closure) = closure_candidate {
                        let defaults: Vec<(String, Option<ast::Expr>)> = closure
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), p.default.clone()))
                            .collect();
                        if defaults.iter().any(|(_, d)| d.is_some()) {
                            ctx.closure_defaults.insert(name.clone(), defaults);
                        }
                    }
                }
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
            ast::Pattern::Tuple(_, _) => {
                // Tuple destructuring: let [a, b] = tuple_expr;
                let tir_pattern = self.resolve_let_pattern(
                    &let_stmt.pattern,
                    type_id,
                    let_stmt.is_mut,
                    let_stmt.span,
                    ctx,
                );
                TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern: tir_pattern,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Struct { .. } => {
                // Struct destructuring: let { x, y } = struct_expr;
                let tir_pattern = self.resolve_let_pattern(
                    &let_stmt.pattern,
                    type_id,
                    let_stmt.is_mut,
                    let_stmt.span,
                    ctx,
                );
                TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern: tir_pattern,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Wildcard => {
                // `let _ = expr;` — evaluate the value for side effects, then
                // discard the result. Lower to LetDestructure with a Wildcard
                // pattern (rather than a bare `TirStmtKind::Expr(value)`) so
                // that the surrounding block's type inference does not treat
                // the discarded expression as a trailing block-value
                // (`Expr::Block` elaborator picks up trailing `TirStmtKind::Expr`
                // as the block's type).
                TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern: TirPattern::Wildcard,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            _ => {
                self.check_irrefutable_pattern(&let_stmt.pattern, let_stmt.span);
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
        }
    }

    /// Resolve an uninitialized let declaration: `let x: T;`
    ///
    /// Emits a `TirStmtKind::Let` with a unit placeholder value so that
    /// the local is pre-allocated (Wasm zero-initializes locals) without
    /// emitting a `LocalSet`.  The bind phase has already verified that the
    /// variable is assigned before any use.
    fn resolve_uninit_let(&mut self, let_stmt: &LetStmt, ctx: &mut FunctionContext) -> TirStmt {
        // Type annotation is guaranteed by the parser when there is no initializer.
        let type_id = self.resolve_type(
            let_stmt
                .ty
                .as_ref()
                .expect("parser ensures type annotation for uninit let"),
        );

        match &let_stmt.pattern {
            ast::Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | ast::Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                let is_mut =
                    let_stmt.is_mut || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                let local_index = ctx.add_local(name.clone(), type_id, is_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, is_mut, type_id);
                // Unit placeholder: WIR builder sees unit type → skips LocalSet.
                // The local is pre-declared and Wasm zero-initializes it.
                let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, let_stmt.span);
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
            _ => {
                self.check_irrefutable_pattern(&let_stmt.pattern, let_stmt.span);
                TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Unit,
                        TypeTable::UNIT,
                        let_stmt.span,
                    )),
                    let_stmt.span,
                )
            }
        }
    }

    /// Check that a pattern is irrefutable (guaranteed to match).
    ///
    /// Irrefutable patterns bind variables unconditionally and are valid in `let` bindings
    /// and `for` loops. Refutable patterns may fail to match and are only valid in `match`
    /// arms, `if let`, and `while let`.
    ///
    /// Emits a compile error and returns `false` if the pattern is refutable.
    fn check_irrefutable_pattern(&mut self, pattern: &ast::Pattern, span: Span) -> bool {
        match pattern {
            // `Pattern::Error` is a parser recovery placeholder; treat it as
            // irrefutable so it does not cascade a second "refutable" error.
            Pattern::Ident { .. }
            | Pattern::MutIdent { .. }
            | Pattern::Wildcard
            | Pattern::Error(_) => true,
            Pattern::Tuple(patterns, _) => patterns
                .iter()
                .all(|p| self.check_irrefutable_pattern(p, span)),
            Pattern::Struct { fields, .. } => fields
                .iter()
                .all(|f| self.check_irrefutable_pattern(&f.pattern, span)),
            Pattern::Literal(_) => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "refutable pattern in `let` binding: literal patterns may not match; use `if let` instead".to_string(),
                    span,
                });
                false
            }
            Pattern::Variant { variant_name, .. } => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: format!("refutable pattern in `let` binding: `{variant_name}` may not match; use `if let` instead"),
                    span,
                });
                false
            }
            Pattern::Or(_) => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "refutable pattern in `let` binding: or-patterns may not match; use `if let` or `match` instead".to_string(),
                    span,
                });
                false
            }
            Pattern::Range { .. } => {
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "refutable pattern in `let` binding: range patterns may not match; use `if let` instead".to_string(),
                    span,
                });
                false
            }
        }
    }

    fn is_known_case_of_type(
        &mut self,
        type_id: TypeId,
        case_name: &str,
        qualifier: Option<&Type>,
    ) -> bool {
        if !self.pattern_qualifier_matches_scrutinee(type_id, qualifier) {
            return false;
        }
        let resolved = self.tysys.type_table.borrow().get(type_id).clone();
        match &resolved {
            ResolvedType::Enum { name, .. } => self
                .lookup_enum_case(name)
                .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name)),
            ResolvedType::Variant { name, .. } => self
                .lookup_variant_case(name)
                .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name)),
            ResolvedType::GenericInstance { name, .. } => self
                .lookup_variant_case(name)
                .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name)),
            _ => false,
        }
    }

    fn pattern_qualifier_matches_scrutinee(
        &mut self,
        scrutinee_type: TypeId,
        qualifier: Option<&Type>,
    ) -> bool {
        let Some(qualifier) = qualifier else {
            return true;
        };
        let scrutinee_resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        let (scrutinee_name, scrutinee_module, scrutinee_arg_len) = match &scrutinee_resolved {
            ResolvedType::Enum {
                name,
                module_source,
            }
            | ResolvedType::Variant {
                name,
                module_source,
            } => (name.as_str(), module_source, None),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (name.as_str(), module_source, Some(type_args.len())),
            _ => {
                return false;
            }
        };
        match qualifier {
            Type::Named(t) => t.name == scrutinee_name,
            Type::Generic(g) => {
                g.name == scrutinee_name && scrutinee_arg_len.is_none_or(|n| n == g.args.len())
            }
            Type::NamespacedGeneric(ns) => {
                ns.name == scrutinee_name
                    && scrutinee_arg_len.is_none_or(|n| n == ns.args.len())
                    && self
                        .sem
                        .imports
                        .namespace_imports
                        .get(&ns.namespace)
                        .is_some_and(|m| m == scrutinee_module)
            }
            Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _)
            | Type::Error(_) => false,
        }
    }

    fn format_pattern_case_name(&self, case_name: &str, qualifier: Option<&Type>) -> String {
        let Some(qualifier) = qualifier else {
            return case_name.to_string();
        };
        format!("{}::{case_name}", format_pattern_qualifier_type(qualifier))
    }

    /// Build the lookup key for `associated_constants`, matching how the map was
    /// populated via `get_type_name` (base name only, no generic type arguments).
    ///
    /// For example, `Maybe<i32>::CONST` must look up as `"Maybe::CONST"` because
    /// `get_type_name` strips generic args when building the key.
    fn format_assoc_const_key(variant_name: &str, qualifier: Option<&Type>) -> String {
        let Some(qualifier) = qualifier else {
            return variant_name.to_string();
        };
        let base = match qualifier {
            Type::Named(t) => t.name.as_str(),
            Type::Generic(t) => t.name.as_str(),
            Type::NamespacedGeneric(t) => t.name.as_str(),
            Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _)
            | Type::Error(_) => return variant_name.to_string(),
        };
        format!("{base}::{variant_name}")
    }

    /// Resolve a let pattern (for tuple/struct destructuring).
    /// Applies match ergonomics: if `type_id` is `&T` or `&mut T` and the pattern is
    /// a compound pattern (tuple/struct), peels the reference and wraps bindings.
    pub(super) fn resolve_let_pattern(
        &mut self,
        pattern: &ast::Pattern,
        type_id: TypeId,
        is_mut: bool,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        // Match ergonomics for let patterns: peel references from the type
        // when the pattern is a compound (tuple/struct) pattern.
        let (peeled_type, ref_binding) = match pattern {
            ast::Pattern::Tuple(_, _) | ast::Pattern::Struct { .. } => {
                let mut current = type_id;
                let mut rb = RefBinding::None;
                while let resolved @ (ResolvedType::Ref(_) | ResolvedType::MutRef(_)) =
                    self.tysys.type_table.borrow().get(current).clone()
                {
                    match resolved {
                        ResolvedType::Ref(inner) => {
                            current = inner;
                            rb = RefBinding::Ref;
                        }
                        ResolvedType::MutRef(inner) => {
                            current = inner;
                            if rb == RefBinding::None {
                                rb = RefBinding::MutRef;
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                (current, rb)
            }
            _ => (type_id, RefBinding::None),
        };
        self.resolve_let_pattern_inner(pattern, peeled_type, is_mut, span, ctx, ref_binding)
    }

    fn resolve_let_pattern_inner(
        &mut self,
        pattern: &ast::Pattern,
        type_id: TypeId,
        is_mut: bool,
        span: Span,
        ctx: &mut FunctionContext,
        ref_binding: RefBinding,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | ast::Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                let pat_mut = is_mut || matches!(pattern, ast::Pattern::MutIdent { .. });
                let binding_type = match ref_binding {
                    RefBinding::Ref => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(type_id)),
                    RefBinding::MutRef => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::MutRef(type_id)),
                    RefBinding::None => type_id,
                };
                let local_index = ctx.add_local(name.clone(), binding_type, pat_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, pat_mut, binding_type);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id: binding_type,
                }
            }
            ast::Pattern::Tuple(patterns, has_rest) => {
                // Get element types from the tuple type
                let elem_types = {
                    let type_table = self.tysys.type_table.borrow();
                    if let Some(elem_types) = type_table.as_tuple(type_id) {
                        elem_types
                    } else {
                        // Error: expected tuple type
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: "tuple type".to_string(),
                            found: type_table.type_name(type_id),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    }
                };

                // Check length
                if *has_rest {
                    if patterns.len() > elem_types.len() {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: format!("tuple with at least {} elements", patterns.len()),
                            found: format!("tuple with {} elements", elem_types.len()),
                            span,
                        });
                    }
                } else if patterns.len() != elem_types.len() {
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected: format!("tuple with {} elements", elem_types.len()),
                        found: format!("pattern with {} elements", patterns.len()),
                        span,
                    });
                }

                // Resolve each sub-pattern with its corresponding element type
                let tir_patterns: Vec<TirPattern> = patterns
                    .iter()
                    .zip(
                        elem_types
                            .iter()
                            .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                    )
                    .map(|(p, &elem_type)| {
                        self.resolve_let_pattern_inner(p, elem_type, is_mut, span, ctx, ref_binding)
                    })
                    .collect();

                TirPattern::Tuple(tir_patterns, *has_rest)
            }
            ast::Pattern::Struct {
                type_name,
                fields,
                has_rest,
                span: pat_span,
            } => {
                // Get struct name from type
                let struct_name = {
                    let type_table = self.tysys.type_table.borrow();
                    match type_table.get(type_id) {
                        ResolvedType::Struct { name, .. } => Some(name.clone()),
                        _ => None,
                    }
                };

                // If named pattern, verify the type matches
                if let Some(expected_name) = type_name
                    && let Some(actual_name) = &struct_name
                {
                    // Compare the short name (strip module prefix if needed,
                    // and any `<ns>::` namespace-import prefix the user wrote).
                    let expected_short = self
                        .strip_ns_prefix(expected_name)
                        .unwrap_or(expected_name.as_str());
                    let actual_short = actual_name.rsplit("::").next().unwrap_or(actual_name);
                    if actual_short != expected_short {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: expected_name.clone(),
                            found: self.tysys.type_table.borrow().type_name(type_id),
                            span: *pat_span,
                        });
                    }
                }

                if struct_name.is_none() {
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected: "struct type".to_string(),
                        found: self.tysys.type_table.borrow().type_name(type_id),
                        span: *pat_span,
                    });
                    return TirPattern::Wildcard;
                }

                // Resolve each field pattern
                let mut tir_fields = Vec::new();
                for field in fields {
                    let (field_index, field_type) =
                        self.lookup_field_type(type_id, &field.field_name, field.span);
                    let sub_pattern = self.resolve_let_pattern_inner(
                        &field.pattern,
                        field_type,
                        is_mut,
                        field.span,
                        ctx,
                        ref_binding,
                    );
                    tir_fields.push(TirStructPatternField {
                        field_name: field.field_name.clone(),
                        field_index,
                        pattern: sub_pattern,
                    });
                }

                // Exhaustiveness check: without `..`, all fields must be listed
                if !has_rest
                    && let Some(ref sname) = struct_name
                    && let Some(struct_info) = self.lookup_struct_fields(sname)
                {
                    let total_fields = struct_info.fields.len();
                    if fields.len() != total_fields {
                        let missing: Vec<_> = struct_info
                            .fields
                            .iter()
                            .filter(|(name, _, _)| !fields.iter().any(|f| f.field_name == *name))
                            .map(|(name, _, _)| name.clone())
                            .collect();
                        if !missing.is_empty() {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                        expected: format!(
                                            "all fields (missing: {}), or use `..` to ignore remaining fields",
                                            missing.join(", ")
                                        ),
                                        found: format!(
                                            "pattern with {} of {} fields",
                                            fields.len(),
                                            total_fields
                                        ),
                                        span: *pat_span,
                                    });
                        }
                    }
                }

                TirPattern::Struct {
                    struct_type: type_id,
                    fields: tir_fields,
                    has_rest: *has_rest,
                }
            }
            ast::Pattern::Wildcard => TirPattern::Wildcard,
            ast::Pattern::Literal(_) | ast::Pattern::Variant { .. } => {
                // Refutable pattern: error was already emitted by check_irrefutable_pattern.
                TirPattern::Wildcard
            }
            ast::Pattern::Or(_) | ast::Pattern::Range { .. } => {
                // Refutable pattern: error was already emitted by check_irrefutable_pattern.
                TirPattern::Wildcard
            }
            // Parser error-recovery placeholder; inert.
            ast::Pattern::Error(_) => TirPattern::Wildcard,
        }
    }

    /// Resolve an expression statement
    pub(super) fn resolve_expr_stmt(
        &mut self,
        expr_stmt: &ExprStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let expr = self.resolve_expr(&expr_stmt.expr, ctx, None);
        TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span)
    }

    /// Resolve a return statement
    pub(super) fn resolve_return(
        &mut self,
        ret_stmt: &ReturnStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        // In async functions, `return expr` (with a value) is forbidden; use `task return expr`
        if ctx.is_async && ret_stmt.value.is_some() {
            let _ = self.logger.error(TypeError::InvalidLiteral {
                message:
                    "cannot use `return expr` in `export async fn`; use `task return expr` instead"
                        .to_string(),
                span: ret_stmt.span,
            });
        }
        let return_type = ctx.return_type;
        let value = ret_stmt.value.as_ref().map(|expr| {
            // Use expected type for coercion (numeric literals, tuple to array, etc.)
            self.resolve_expr(expr, ctx, Some(return_type))
        });

        // Check return value type matches function return type
        if let Some(value) = &value {
            self.typecheck_return(value.type_id, return_type, ret_stmt.span);
        }

        TirStmt::new(TirStmtKind::Return { value }, ret_stmt.span)
    }

    /// Resolve a `task return` statement
    pub(super) fn resolve_task_return(
        &mut self,
        tr_stmt: &TaskReturnStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        if !ctx.is_async {
            let _ = self.logger.error(TypeError::InvalidLiteral {
                message: "`task return` is only valid inside `export async fn`".to_string(),
                span: tr_stmt.span,
            });
        }
        let expected = ctx.task_return_type;
        let value = self.resolve_expr(&tr_stmt.value, ctx, expected);
        TirStmt::new(TirStmtKind::TaskReturn { value }, tr_stmt.span)
    }

    /// Resolve an if statement
    /// Returns Vec<TirStmt> to handle if-let-init scoping: let binding + if statement
    pub(super) fn resolve_if_stmt(
        &mut self,
        if_stmt: &IfStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        match &if_stmt.condition {
            ast::Condition::Expr(expr) => {
                let condition = self.resolve_expr(expr, ctx, Some(TypeTable::BOOL));
                let then_block = self.resolve_block(&if_stmt.then_block, ctx, None);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx, None));
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
                self.record_desugar(if_stmt.id, super::sem::types::DesugarKind::IfLetChain);
                // Resolve else_block in outer scope (chain bindings are not visible there)
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx, None));

                // Enter scope for chain element bindings and then_block
                ctx.enter_scope();
                let stmts = self.resolve_let_chain_stmts(
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

    /// Like `resolve_if_stmt` but propagates `expected_type` to blocks for coercion.
    /// Used when an if statement is the last statement in a block that needs type coercion
    /// (e.g., match arm returning List<T> from an if-else with tuple literals).
    fn resolve_if_stmt_with_expected(
        &mut self,
        if_stmt: &IfStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> Vec<TirStmt> {
        match &if_stmt.condition {
            ast::Condition::Expr(expr) => {
                let condition = self.resolve_expr(expr, ctx, Some(TypeTable::BOOL));
                let then_block = self.resolve_block(&if_stmt.then_block, ctx, expected_type);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx, expected_type));
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
                self.record_desugar(if_stmt.id, super::sem::types::DesugarKind::IfLetChain);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx, expected_type));

                ctx.enter_scope();
                let stmts = self.resolve_let_chain_stmts(
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
        }
    }

    /// Resolve a let-chain condition into a nested sequence of TIR statements.
    ///
    /// Each element of the chain adds one nesting level: a `Let` element becomes a
    /// two-arm `Match` expression statement (the pattern arm vs. a wildcard else
    /// arm; the scrutinee stays inline and is hoisted into a temp local later, in
    /// `translate::pattern`), and an `Expr` element becomes an `If` node (boolean
    /// guard). All levels that fail fall through to `else_block`; the innermost
    /// level runs `then_block`.
    ///
    /// The `else_block` TIR is cloned for each failure path. This duplicates else-block code
    /// in the output, but is typically small (e.g., `None` or a single `panic` call).
    pub(super) fn resolve_let_chain_stmts(
        &mut self,
        elements: &[ConditionElement],
        then_block_ast: &ast::Block,
        else_block: Option<&TirBlock>,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        span: Span,
    ) -> Vec<TirStmt> {
        if elements.is_empty() {
            return self.resolve_block(then_block_ast, ctx, expected_type).stmts;
        }
        // Process the current element first so its bindings are visible when resolving
        // subsequent elements and the then_block (via recursive calls below).
        match &elements[0] {
            ConditionElement::Let {
                pattern,
                expr,
                span: elem_span,
            } => {
                let scrutinee = self.resolve_expr(expr, ctx, None);
                let scrutinee_type = scrutinee.type_id;
                // Adds pattern bindings to ctx — subsequent elements can see them
                let tir_pattern = self.resolve_if_pattern(pattern, scrutinee_type, ctx, *elem_span);
                let inner_block = TirBlock::new(
                    self.resolve_let_chain_stmts(
                        &elements[1..],
                        then_block_ast,
                        else_block,
                        ctx,
                        expected_type,
                        span,
                    ),
                    span,
                );
                // `if let` is normalized to a two-arm `Match` — NIR's
                // canonical pattern-matching form. The scrutinee stays
                // inline: `translate::pattern` hoists it into a temp
                // local later, after `resource_cleanup` has run, so the
                // resource-flow analysis sees the same shape it saw for
                // the old `IfLet` node.
                let then_type = Self::block_result_type(&inner_block);
                let else_tir = else_block.cloned();
                let else_type = else_tir
                    .as_ref()
                    .map_or(TypeTable::UNIT, Self::block_result_type);
                let else_arm_span = else_tir.as_ref().map_or(span, |b| b.span);
                // Equal types agree; a `Never` arm defers to the other;
                // an outright mismatch (statement-position `if let` whose
                // then-block ends in a non-Unit call) falls back to
                // `Unit`, letting both arm values be dropped.
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
            ConditionElement::Expr(expr) => {
                let condition = self.resolve_expr(expr, ctx, Some(TypeTable::BOOL));
                let inner_block = TirBlock::new(
                    self.resolve_let_chain_stmts(
                        &elements[1..],
                        then_block_ast,
                        else_block,
                        ctx,
                        expected_type,
                        span,
                    ),
                    span,
                );
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

    /// Resolve a pattern in an if-pattern context with type information from the scrutinee.
    /// Match ergonomics: if the scrutinee is `&T`, peels the reference and propagates
    /// `ref_binding` so that identifier bindings get `&InnerType` instead of `InnerType`.
    pub(super) fn resolve_if_pattern(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirPattern {
        let mut peeled_type = scrutinee_type;
        let mut ref_binding = RefBinding::None;
        while let resolved @ (ResolvedType::Ref(_) | ResolvedType::MutRef(_)) =
            self.tysys.type_table.borrow().get(peeled_type).clone()
        {
            match resolved {
                ResolvedType::Ref(inner) => {
                    peeled_type = inner;
                    // &T always downgrades to Ref (most restrictive wins)
                    ref_binding = RefBinding::Ref;
                }
                ResolvedType::MutRef(inner) => {
                    peeled_type = inner;
                    // &mut T only sets MutRef if not already downgraded to Ref
                    if ref_binding == RefBinding::None {
                        ref_binding = RefBinding::MutRef;
                    }
                }
                _ => unreachable!(),
            }
        }
        self.resolve_if_pattern_inner(pattern, peeled_type, ctx, span, ref_binding)
    }

    fn resolve_if_pattern_inner(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        span: Span,
        ref_binding: RefBinding,
    ) -> TirPattern {
        match pattern {
            Pattern::Wildcard => TirPattern::Wildcard,
            Pattern::Ident {
                id,
                name,
                span: name_span,
            }
            | Pattern::MutIdent {
                id,
                name,
                span: name_span,
            } => {
                // A bare identifier in a pattern context could be a variant/enum case
                // (e.g., `None`, `Red`) or a variable binding (e.g., `x`, `val`).
                // The parser does not use case to disambiguate; instead, we check
                // whether the name is a known case of the scrutinee type.
                if !matches!(pattern, Pattern::MutIdent { .. })
                    && self.is_known_case_of_type(scrutinee_type, name, None)
                {
                    // Delegate to the Variant branch with empty bindings.
                    // Preserve the identifier's AstId/span as name_id/name_span so
                    // LSP jump-to-def on `None`/`Red` still resolves to the case decl.
                    return self.resolve_if_pattern_inner(
                        &Pattern::Variant {
                            variant_name: name.clone(),
                            variant_qualifier: None,
                            name_id: Some(*id),
                            name_span: *name_span,
                            bindings: vec![],
                            span,
                        },
                        scrutinee_type,
                        ctx,
                        span,
                        ref_binding,
                    );
                }
                // Check if the identifier refers to an immutable global constant
                if !matches!(pattern, Pattern::MutIdent { .. }) {
                    if let Some(&(ty, mutable)) = self.sem.decls.current_module_globals.get(name)
                        && !mutable
                    {
                        return TirPattern::ConstantValue {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::GlobalVarGet {
                                    module_source: self.current_module_source.clone(),
                                    name: name.clone(),
                                },
                                ty,
                                span,
                            )),
                        };
                    }
                    if let Some((source_module, original_name, ty, mutable)) =
                        self.sem.decls.imported_globals.get(name)
                        && !*mutable
                    {
                        return TirPattern::ConstantValue {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::GlobalVarGet {
                                    module_source: source_module.clone(),
                                    name: original_name.clone(),
                                },
                                *ty,
                                span,
                            )),
                        };
                    }
                }
                let is_mut = matches!(pattern, Pattern::MutIdent { .. });
                let binding_type = match ref_binding {
                    RefBinding::Ref => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(scrutinee_type)),
                    RefBinding::MutRef => self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::MutRef(scrutinee_type)),
                    RefBinding::None => scrutinee_type,
                };
                let index = ctx.add_local(name.clone(), binding_type, is_mut, Some(*id));
                self.record_local_symbol(*id, name, *name_span, is_mut, binding_type);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index: index,
                    type_id: binding_type,
                }
            }
            Pattern::Literal(lit) => {
                let tir_lit = match lit {
                    Literal::Number(repr) => {
                        // Float literals cannot be used in match patterns
                        if util::is_float_only_literal(repr) {
                            let _ = self.logger.error(TypeError::InvalidPattern {
                                message: "float literals cannot be used in match patterns"
                                    .to_string(),
                                span,
                            });
                            return TirPattern::Wildcard;
                        }
                        // Check if scrutinee type is unsigned
                        let scrutinee_resolved =
                            self.tysys.type_table.borrow().get(scrutinee_type).clone();
                        let is_unsigned = matches!(
                            scrutinee_resolved,
                            ResolvedType::Primitive(
                                PrimitiveType::U8
                                    | PrimitiveType::U16
                                    | PrimitiveType::U32
                                    | PrimitiveType::U64
                                    | PrimitiveType::U128
                            )
                        ) || matches!(
                            scrutinee_resolved,
                            ResolvedType::Struct { ref name, .. } if name == "u128"
                        );
                        if is_unsigned {
                            match util::parse_u128_literal(repr) {
                                Ok(value) => TirLiteralPattern::U128(value),
                                Err(_) => TirLiteralPattern::U128(0),
                            }
                        } else {
                            match util::parse_i128_literal(repr) {
                                Ok(value) => TirLiteralPattern::I128(value),
                                Err(_) => TirLiteralPattern::I128(0),
                            }
                        }
                    }
                    Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    Literal::Char(raw) => {
                        TirLiteralPattern::Char(util::unescape_char(raw).unwrap_or('\0'))
                    }
                    Literal::String(raw) => {
                        TirLiteralPattern::String(util::unescape_string(raw).unwrap_or_default())
                    }
                    Literal::Null => {
                        // If the scrutinee is a variant type with a `None` case,
                        // lower `null` to a variant pattern for `None`
                        if let Some(none_pattern) = self.try_null_as_none_pattern(scrutinee_type) {
                            return none_pattern;
                        }
                        TirLiteralPattern::Null
                    }
                    _ => TirLiteralPattern::Null,
                };
                TirPattern::Literal(tir_lit)
            }
            Pattern::Tuple(patterns, has_rest) => {
                // For tuple patterns, extract element types
                let element_types =
                    if let Some(types) = self.tysys.type_table.borrow().as_tuple(scrutinee_type) {
                        types
                    } else {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: "tuple type".to_string(),
                            found: self.tysys.type_table.borrow().type_name(scrutinee_type),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    };

                let resolved: Vec<TirPattern> = patterns
                    .iter()
                    .zip(
                        element_types
                            .iter()
                            .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                    )
                    .map(|(p, &ty)| self.resolve_if_pattern_inner(p, ty, ctx, span, ref_binding))
                    .collect();
                TirPattern::Tuple(resolved, *has_rest)
            }
            Pattern::Variant {
                variant_name,
                variant_qualifier,
                name_id,
                name_span,
                bindings,
                span,
            } => {
                // `<ns>::<Case>` (single `::`, prefix is a namespace import
                // alias) canonicalizes to the bare `<Case>`; the registries
                // below are keyed by canonical names. Multi-segment forms
                // (`<ns>::<Type>::<case>`) reach pattern resolution as a
                // `variant_qualifier` Type, not embedded in `variant_name`.
                let normalized_variant_name = self
                    .strip_ns_prefix(variant_name)
                    .unwrap_or(variant_name.as_str());
                let qualified_variant_name =
                    self.format_pattern_case_name(variant_name, variant_qualifier.as_ref());
                // Bare uppercase identifier that is not a known case of the scrutinee type.
                // Check if it's an associated constant (e.g., `i32::MAX`) before falling back
                // to a variable binding.
                if bindings.is_empty()
                    && !self.is_known_case_of_type(
                        scrutinee_type,
                        normalized_variant_name,
                        variant_qualifier.as_ref(),
                    )
                {
                    // Check for associated constants (e.g., `i32::MAX`, `f64::PI`).
                    // Use the base type name (no generic args) to match how
                    // `associated_constants` keys are built via `get_type_name`.
                    let assoc_const_key =
                        Self::format_assoc_const_key(variant_name, variant_qualifier.as_ref());
                    // Resolve to literal patterns when possible for switch optimization.
                    if let Some((_const_module, type_id, const_expr)) = self
                        .sem
                        .decls
                        .associated_constants
                        .get(&assoc_const_key)
                        .cloned()
                    {
                        let resolved = self.resolve_expr(&const_expr, ctx, Some(type_id));
                        // If the resolved expression is a literal, emit a Literal pattern
                        // so it benefits from switch optimization and exhaustiveness checking.
                        match &resolved.kind {
                            TirExprKind::IntLiteral { repr, .. } => {
                                let scrutinee_resolved =
                                    self.tysys.type_table.borrow().get(scrutinee_type).clone();
                                let is_unsigned = matches!(
                                    scrutinee_resolved,
                                    ResolvedType::Primitive(
                                        PrimitiveType::U8
                                            | PrimitiveType::U16
                                            | PrimitiveType::U32
                                            | PrimitiveType::U64
                                            | PrimitiveType::U128
                                    )
                                ) || matches!(
                                    scrutinee_resolved,
                                    ResolvedType::Struct { ref name, .. } if name == "u128"
                                );
                                if is_unsigned {
                                    if let Ok(v) = util::parse_u128_literal(repr) {
                                        return TirPattern::Literal(TirLiteralPattern::U128(v));
                                    }
                                } else if let Ok(v) = util::parse_i128_literal(repr) {
                                    return TirPattern::Literal(TirLiteralPattern::I128(v));
                                }
                            }
                            TirExprKind::BoolLiteral(v) => {
                                return TirPattern::Literal(TirLiteralPattern::Bool(*v));
                            }
                            TirExprKind::CharLiteral(v) => {
                                return TirPattern::Literal(TirLiteralPattern::Char(*v));
                            }
                            _ => {}
                        }
                        return TirPattern::ConstantValue {
                            expr: Box::new(TirExpr::new(resolved.kind, type_id, *span)),
                        };
                    }

                    let binding_type = match ref_binding {
                        RefBinding::Ref => self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(scrutinee_type)),
                        RefBinding::MutRef => self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::MutRef(scrutinee_type)),
                        RefBinding::None => scrutinee_type,
                    };
                    let index =
                        ctx.add_local(qualified_variant_name.clone(), binding_type, false, None);
                    return TirPattern::Binding {
                        name: qualified_variant_name,
                        local_index: index,
                        type_id: binding_type,
                    };
                }

                let resolved_type = self.tysys.type_table.borrow().get(scrutinee_type).clone();
                if !self
                    .pattern_qualifier_matches_scrutinee(scrutinee_type, variant_qualifier.as_ref())
                {
                    let expected = match &resolved_type {
                        ResolvedType::Enum { name, .. } => format!("valid case of enum {name}"),
                        ResolvedType::Variant { name, .. }
                        | ResolvedType::GenericInstance { name, .. } => {
                            format!("valid case of variant {name}")
                        }
                        _ => "variant or enum case".to_string(),
                    };
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected,
                        found: qualified_variant_name,
                        span: *span,
                    });
                    return TirPattern::Wildcard;
                }

                // Handle enum types (no payload, just discriminant matching)
                if let ResolvedType::Enum { name, .. } = &resolved_type {
                    if !bindings.is_empty() {
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!("enum case `{variant_name}` does not have a payload"),
                            span: *span,
                        });
                    }
                    // Look up the enum case index
                    if let Some(enum_info) = self.lookup_enum_case(name).cloned() {
                        if let Some(case_data) =
                            enum_info.find_case(normalized_variant_name).cloned()
                        {
                            // Record pattern's case-name identifier -> enum case decl
                            if let Some(id) = name_id {
                                self.record_reference_to_decl(
                                    *id,
                                    &enum_info.module_source,
                                    case_data.ast_id,
                                );
                            }
                            let _ = name_span;
                            return TirPattern::Enum {
                                enum_type: scrutinee_type,
                                case_name: normalized_variant_name.to_string(),
                                case_index: case_data.index,
                            };
                        }
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: format!(
                                "one of: {}",
                                enum_info
                                    .cases
                                    .iter()
                                    .map(|c| c.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            found: self
                                .format_pattern_case_name(variant_name, variant_qualifier.as_ref()),
                            span: *span,
                        });
                        return TirPattern::Wildcard;
                    }
                    let _ = self.logger.error(TypeError::PatternTypeMismatch {
                        expected: format!("enum type `{name}`"),
                        found: "unknown enum".to_string(),
                        span: *span,
                    });
                    return TirPattern::Wildcard;
                }

                // Record use->def for the variant case name in the pattern
                // (e.g., `Some` in `Some(x)`). Points at the case declaration's
                // span so LSP jump-to-def from the pattern lands on the case decl.
                let variant_type_name: Option<String> = match &resolved_type {
                    ResolvedType::Variant { name, .. } => Some(name.clone()),
                    ResolvedType::GenericInstance { name, .. } if self.contains_variant(name) => {
                        Some(name.clone())
                    }
                    _ => None,
                };
                if let Some(id) = name_id
                    && let Some(type_name) = variant_type_name.as_ref()
                    && let Some(variant_info) = self.lookup_variant_case(type_name).cloned()
                    && let Some(case_data) = variant_info
                        .cases
                        .iter()
                        .find(|c| c.name == normalized_variant_name)
                        .cloned()
                {
                    self.record_reference_to_decl(
                        *id,
                        &variant_info.module_source,
                        case_data.ast_id,
                    );
                }
                let _ = name_span;

                // Each variant case has exactly one payload type.
                // Determine the payload type for the variant case.
                let payload_type: TypeId = match &resolved_type {
                    // Non-generic variant
                    ResolvedType::Variant { name, .. } => self.get_variant_case_payload_type(
                        name,
                        normalized_variant_name,
                        &[],
                        *span,
                    ),
                    // Generic variant instantiation
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        // Check if this is a variant (not a struct)
                        if self.contains_variant(name) {
                            self.get_variant_case_payload_type(
                                name,
                                normalized_variant_name,
                                type_args,
                                *span,
                            )
                        } else {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                expected: "variant type".to_string(),
                                found: name.clone(),
                                span: *span,
                            });
                            TypeTable::UNKNOWN
                        }
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::PatternTypeMismatch {
                            expected: "variant or enum type".to_string(),
                            found: format!("{resolved_type:?}"),
                            span: *span,
                        });
                        TypeTable::UNKNOWN
                    }
                };

                // Single payload = single binding pattern.
                // For backward compatibility, we still accept `Some(x)` as single binding.
                let resolved_bindings: Vec<TirPattern> = if bindings.len() == 1 {
                    vec![self.resolve_if_pattern_inner(
                        &bindings[0],
                        payload_type,
                        ctx,
                        *span,
                        ref_binding,
                    )]
                } else if bindings.is_empty() {
                    // Unit case like `None` - no bindings
                    vec![]
                } else {
                    // Multiple bindings are deprecated with single payload design.
                    // Error will be caught by test fixture updates.
                    bindings
                        .iter()
                        .map(|p| {
                            self.resolve_if_pattern_inner(
                                p,
                                TypeTable::UNKNOWN,
                                ctx,
                                *span,
                                ref_binding,
                            )
                        })
                        .collect()
                };

                TirPattern::Variant {
                    enum_type: scrutinee_type,
                    variant_name: normalized_variant_name.to_string(),
                    bindings: resolved_bindings,
                    payload_type,
                }
            }
            Pattern::Struct {
                type_name,
                fields,
                has_rest,
                span: pat_span,
            } => {
                // Verify type if named
                if let Some(expected_name) = type_name {
                    let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
                    if let ResolvedType::Struct { ref name, .. } = resolved {
                        let expected_short = self
                            .strip_ns_prefix(expected_name)
                            .unwrap_or(expected_name.as_str());
                        let actual_short = name.rsplit("::").next().unwrap_or(name);
                        if actual_short != expected_short {
                            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                expected: expected_name.clone(),
                                found: self.tysys.type_table.borrow().type_name(scrutinee_type),
                                span: *pat_span,
                            });
                        }
                    }
                }

                let mut tir_fields = Vec::new();
                for field in fields {
                    let (field_index, field_type) =
                        self.lookup_field_type(scrutinee_type, &field.field_name, field.span);
                    let sub_pattern = self.resolve_if_pattern_inner(
                        &field.pattern,
                        field_type,
                        ctx,
                        field.span,
                        ref_binding,
                    );
                    tir_fields.push(TirStructPatternField {
                        field_name: field.field_name.clone(),
                        field_index,
                        pattern: sub_pattern,
                    });
                }

                // Exhaustiveness check
                if !has_rest {
                    let struct_name = {
                        let type_table = self.tysys.type_table.borrow();
                        match type_table.get(scrutinee_type) {
                            ResolvedType::Struct { name, .. } => Some(name.clone()),
                            _ => None,
                        }
                    };
                    if let Some(ref sname) = struct_name
                        && let Some(struct_info) = self.lookup_struct_fields(sname)
                    {
                        let total_fields = struct_info.fields.len();
                        if fields.len() != total_fields {
                            let missing: Vec<_> = struct_info
                                .fields
                                .iter()
                                .filter(|(name, _, _)| {
                                    !fields.iter().any(|f| f.field_name == *name)
                                })
                                .map(|(name, _, _)| name.clone())
                                .collect();
                            if !missing.is_empty() {
                                let _ = self.logger.error(TypeError::PatternTypeMismatch {
                                        expected: format!(
                                            "all fields (missing: {}), or use `..` to ignore remaining fields",
                                            missing.join(", ")
                                        ),
                                        found: format!(
                                            "pattern with {} of {} fields",
                                            fields.len(),
                                            total_fields
                                        ),
                                        span: *pat_span,
                                    });
                            }
                        }
                    }
                }

                TirPattern::Struct {
                    struct_type: scrutinee_type,
                    fields: tir_fields,
                    has_rest: *has_rest,
                }
            }
            Pattern::Or(alternatives) => {
                let mut resolved = Vec::with_capacity(alternatives.len());

                // Resolve first alternative normally
                if let Some(first_alt) = alternatives.first() {
                    let first = self.resolve_if_pattern_inner(
                        first_alt,
                        scrutinee_type,
                        ctx,
                        span,
                        ref_binding,
                    );
                    let first_bindings = collect_pattern_bindings_with_index(&first);
                    resolved.push(first);

                    // Resolve subsequent alternatives and remap their local indices
                    // to match the first alternative's bindings
                    for (i, alt) in alternatives.iter().enumerate().skip(1) {
                        let alt_resolved = self.resolve_if_pattern_inner(
                            alt,
                            scrutinee_type,
                            ctx,
                            span,
                            ref_binding,
                        );
                        let alt_bindings = collect_pattern_bindings_with_index(&alt_resolved);

                        // Validate same names and types
                        let first_names: Vec<(&str, crate::tir::TypeId)> = first_bindings
                            .iter()
                            .map(|(n, _, t)| (n.as_str(), *t))
                            .collect();
                        let alt_names: Vec<(&str, crate::tir::TypeId)> = alt_bindings
                            .iter()
                            .map(|(n, _, t)| (n.as_str(), *t))
                            .collect();

                        if first_names == alt_names {
                            // Remap local indices in the alternative to match the first
                            let mut remapped = alt_resolved;
                            for (first_bind, alt_bind) in
                                first_bindings.iter().zip(alt_bindings.iter())
                            {
                                if first_bind.1 != alt_bind.1 {
                                    remap_pattern_local(&mut remapped, alt_bind.1, first_bind.1);
                                }
                            }
                            resolved.push(remapped);
                        } else {
                            let fn_: Vec<&str> = first_names.iter().map(|(n, _)| *n).collect();
                            let an: Vec<&str> = alt_names.iter().map(|(n, _)| *n).collect();
                            let _ = self.logger.error(TypeError::InvalidPattern {
                                message: format!(
                                    "or-pattern alternatives must bind the same names with the same types: \
                                     alternative 1 binds {:?}, but alternative {} binds {:?}",
                                    fn_, i + 1, an,
                                ),
                                span,
                            });
                            resolved.push(alt_resolved);
                        }
                    }

                    // Update scope entries to use the first alternative's local indices
                    // so the arm body resolves names to the correct locals.
                    //
                    // Also align each binding's `defining_ast_id` with the first
                    // alternative's pattern, so that LSP jump-to-def on a use
                    // inside the arm body points at the first alternative's
                    // binding (the canonical definition site).
                    let mut first_alt_ast_ids: crate::hashmap::IndexMap<String, AstId> =
                        crate::hashmap::IndexMap::default();
                    if let Some(first_alt) = alternatives.first() {
                        collect_ast_pattern_binding_ids(first_alt, &mut first_alt_ast_ids);
                    }
                    for (name, local_index, _type_id) in &first_bindings {
                        if let Some(scope) = ctx.scopes.last_mut()
                            && let Some(var) = scope.get_mut(name)
                        {
                            var.index = *local_index;
                            if let Some(first_id) = first_alt_ast_ids.get(name) {
                                var.defining_ast_id = Some(*first_id);
                            }
                        }
                    }
                }

                TirPattern::Or(resolved)
            }
            Pattern::Range {
                start,
                end,
                kind,
                span: range_span,
            } => self.resolve_range_pattern(start, end, *kind, scrutinee_type, *range_span),
            // Parser error-recovery placeholder; inert.
            Pattern::Error(_) => TirPattern::Wildcard,
        }
    }

    /// If the scrutinee is a variant type that has a `None` case, return
    /// a `TirPattern::Variant` for `None`. Otherwise return `None`.
    fn try_null_as_none_pattern(&self, scrutinee_type: TypeId) -> Option<TirPattern> {
        let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        let variant_name = match &resolved {
            ResolvedType::Variant { name, .. } => Some(name.clone()),
            ResolvedType::GenericInstance { name, .. } if self.contains_variant(name) => {
                Some(name.clone())
            }
            _ => None,
        }?;
        let variant_info = self.lookup_variant_case(&variant_name)?;
        let none_case_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .variant_case_name(crate::compiler_item::CompilerItem::OptionNone)
            .to_string();
        if variant_info.cases.iter().any(|c| c.name == none_case_name) {
            Some(TirPattern::Variant {
                enum_type: scrutinee_type,
                variant_name: none_case_name,
                bindings: vec![],
                payload_type: TypeTable::UNIT,
            })
        } else {
            None
        }
    }

    /// Resolve a range pattern: `0..<10` or `'a'..='z'`
    fn resolve_range_pattern(
        &mut self,
        start: &Pattern,
        end: &Pattern,
        kind: crate::ast::RangeKind,
        scrutinee_type: TypeId,
        span: Span,
    ) -> TirPattern {
        let scrutinee_resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
        let is_unsigned = matches!(
            scrutinee_resolved,
            ResolvedType::Primitive(
                PrimitiveType::U8
                    | PrimitiveType::U16
                    | PrimitiveType::U32
                    | PrimitiveType::U64
                    | PrimitiveType::U128
            )
        ) || matches!(
            scrutinee_resolved,
            ResolvedType::Struct { ref name, .. } if name == "u128"
        );

        let start_val = self.pattern_to_i128(start, is_unsigned);
        let end_val = self.pattern_to_i128(end, is_unsigned);

        let (Some(start_val), Some(end_val)) = (start_val, end_val) else {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "range pattern bounds must be integer or char literals".to_string(),
                span,
            });
            return TirPattern::Wildcard;
        };

        // Check for reversed or empty range
        let inclusive = matches!(kind, crate::ast::RangeKind::Inclusive);
        if start_val > end_val {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "reversed range pattern".to_string(),
                span,
            });
            return TirPattern::Wildcard;
        }
        if !inclusive && start_val >= end_val {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: "empty range pattern".to_string(),
                span,
            });
            return TirPattern::Wildcard;
        }

        TirPattern::Range {
            start: start_val,
            end: end_val,
            inclusive,
            is_unsigned,
        }
    }

    fn pattern_to_i128(&self, pattern: &Pattern, is_unsigned: bool) -> Option<i128> {
        match pattern {
            Pattern::Literal(Literal::Number(repr)) => {
                if is_unsigned {
                    util::parse_u128_literal(repr).ok().map(|v| v as i128)
                } else {
                    util::parse_i128_literal(repr).ok()
                }
            }
            Pattern::Literal(Literal::Char(raw)) => {
                util::unescape_char(raw).ok().map(|c| c as i128)
            }
            Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                ..
            } if bindings.is_empty() => {
                // Could be an associated constant like i32::MAX, i32::MIN
                self.primitive_assoc_const_to_i128(variant_qualifier.as_ref(), variant_name)
            }
            _ => None,
        }
    }

    fn primitive_assoc_const_to_i128(
        &self,
        qualifier: Option<&Type>,
        const_name: &str,
    ) -> Option<i128> {
        primitive_assoc_const_to_i128(qualifier, const_name)
    }

    /// Get payload type for a variant case, substituting type parameters if needed
    pub(super) fn get_variant_case_payload_type(
        &mut self,
        variant_name: &str,
        case_name: &str,
        type_args: &[TypeId],
        span: Span,
    ) -> TypeId {
        // Clone payload first to avoid borrow conflict with substitute_type_params
        let payload_opt = self.lookup_variant_case(variant_name).and_then(|info| {
            info.cases
                .iter()
                .find(|case| case.name == case_name)
                .map(|case| case.payload)
        });

        if let Some(payload) = payload_opt {
            // Substitute type parameters with concrete types
            return self.substitute_type_params(payload, type_args);
        }

        // Check if variant exists but case not found
        if self.contains_variant(variant_name) {
            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                expected: format!("valid case of variant {variant_name}"),
                found: case_name.to_string(),
                span,
            });
        } else {
            let _ = self.logger.error(TypeError::PatternTypeMismatch {
                expected: "known variant type".to_string(),
                found: variant_name.to_string(),
                span,
            });
        }
        TypeTable::UNKNOWN
    }
    /// Resolve a loop statement (infinite loop).
    ///
    /// Naked `continue` inside this loop's body must jump to the top of
    /// *this* loop, regardless of any enclosing C-style `for` whose
    /// continue-retarget label is still on the stack. Take + restore
    /// `for_continue_labels` around body resolution so the inner
    /// `resolve_continue` sees an empty stack and lowers naturally.
    pub(super) fn resolve_loop(
        &mut self,
        loop_stmt: &LoopStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let saved = std::mem::take(&mut ctx.for_continue_labels);
        let body = self.resolve_block(&loop_stmt.body, ctx, None);
        ctx.for_continue_labels = saved;
        TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)
    }

    /// Resolve a for-of loop.
    ///
    /// For tuples: compile-time expansion (one copy of the body per element).
    /// For non-tuples: iterator pattern via `into_iter()` + `next()`.
    pub(super) fn resolve_for_of(
        &mut self,
        for_of: &ForOfStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let _ = for_of.span;
        // Naked `continue` inside this for-of's body targets *this* loop,
        // not an enclosing C-style `for` body label. The iterable itself
        // is an expression — no `continue` stmt syntactically — but we
        // clear the stack early so all internal resolve_* calls share the
        // same invariant.
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);

        // Check if the iterable is `.enumerate()` on something.
        //
        // Stage 4 / WEP 2026-05-26 note: the `.enumerate()`
        // `MethodCallExpr` is unwrapped here at the AST level — the
        // elaborator never resolves it as a method call, so
        // `expression_types` and `method_dispatch` carry no entry for
        // `mc.id`. The future `reify` pass re-detects this pattern by
        // looking at `for_of.iterable` directly, so missing annotations
        // on `mc` are intentional (mirroring the `tuple.len()` /
        // `.zip()` short-circuits documented on `MethodDispatch`).
        let (actual_iterable, is_enumerate) = match &for_of.iterable {
            Expr::MethodCall(mc) if mc.method == "enumerate" && mc.args.is_empty() => {
                (&mc.receiver, true)
            }
            _ => (&for_of.iterable, false),
        };

        // Resolve the iterable to determine its type
        let iterable = self.resolve_expr(actual_iterable, ctx, None);
        let iterable_type_id = iterable.type_id;

        // Check if it's a tuple type
        let tuple_info = {
            let type_table = self.tysys.type_table.borrow();
            if let Some(elems) = type_table.as_tuple(iterable_type_id) {
                let has_type_pack = elems
                    .iter()
                    .any(|e| matches!(type_table.get(*e), ResolvedType::TypePack { .. }));
                Some((elems, has_type_pack))
            } else {
                None
            }
        };
        // TupleZip with nested TypePacks: treat as variadic so expansion
        // is deferred to monomorphization when concrete types are known.
        let is_zip_variadic = matches!(&iterable.kind, TirExprKind::TupleZip { .. })
            && self.type_contains_pack(iterable_type_id);

        let result = if let Some((elems, has_type_pack)) = tuple_info {
            if has_type_pack || is_zip_variadic {
                assert!(
                    !is_enumerate,
                    "variadic for-of with .enumerate() is not yet supported"
                );
                self.record_desugar(for_of.id, super::sem::types::DesugarKind::ForOfVariadic);
                self.resolve_variadic_for_of(for_of, iterable, ctx)
            } else {
                self.record_desugar(for_of.id, super::sem::types::DesugarKind::ForOfTuple);
                self.resolve_tuple_for_of(for_of, iterable, &elems, is_enumerate, ctx)
            }
        } else {
            // Check that the iterable type implements IntoIterator
            let mut inner_type_id = iterable_type_id;
            while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                self.tysys.type_table.borrow().get(inner_type_id).clone()
            {
                inner_type_id = t;
            }
            let implements_into_iter = self.type_implements_trait(iterable_type_id, "IntoIterator")
                || self.type_implements_trait(inner_type_id, "IntoIterator")
                || matches!(
                    self.tysys.type_table.borrow().get(iterable_type_id),
                    ResolvedType::Unknown | ResolvedType::TypeParam { .. }
                );
            if !implements_into_iter {
                let type_name = self.tysys.type_table.borrow().type_name(iterable_type_id);
                let _ = self.logger.error(TypeError::MissingTraitImpl {
                    type_name,
                    trait_name: "IntoIterator".to_string(),
                    span: for_of.span,
                });
            }
            // Only record the desugar tag when the iterable actually
            // supports iteration; tagging an error-path node would lead
            // Stage 5 reify to expand a TIR shape the elaborator never
            // produced.
            if implements_into_iter {
                self.record_desugar(for_of.id, super::sem::types::DesugarKind::ForOfIterator);
            }
            self.resolve_iterator_for_of(for_of, ctx)
        };

        ctx.for_continue_labels = saved_continue;
        result
    }

    /// Expand `for let v of tuple { body }` by unrolling the body once per element.
    ///
    /// Produces:
    /// ```text
    /// __tuple_for_of_N: {
    ///     let __tuple_N = <iterable>;
    ///     { let v = __tuple_N.0; body }
    ///     { let v = __tuple_N.1; body }
    ///     ...
    /// }
    /// ```
    /// Create a deferred `VariadicForOf` TIR node for `for let v of iterable`
    /// where `iterable` has a tuple type containing `TypePack` elements.
    ///
    /// The body is resolved once with the loop variable having the `TypePack` type.
    /// The monomorphizer will expand this after type substitution resolves the
    /// `TypePack` to a concrete tuple.
    fn resolve_variadic_for_of(
        &mut self,
        for_of: &ForOfStmt,
        iterable: TirExpr,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let span = for_of.span;

        // Validate: no break/continue/return in variadic for-of
        if let Some((kind, bad_span)) = Self::find_control_flow_in_block(&for_of.body) {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: format!(
                    "`{kind}` is not allowed inside a variadic for-of loop (the loop is expanded at compile time)"
                ),
                span: bad_span,
            });
            return vec![TirStmt::new(TirStmtKind::Expr(iterable), span)];
        }

        let unique_id = ctx.next_local;

        // Extract the element type for the loop binding.
        // For direct TypePack: iterable is Tuple([TypePack{T}]), binding type is TypePack.
        // For TupleZip: iterable is Tuple([Tuple([TypePack, TypePack])]), binding type is the inner tuple.
        let binding_type = {
            let type_table = self.tysys.type_table.borrow();
            if let Some(elems) = type_table.as_tuple(iterable.type_id) {
                // Prefer a direct TypePack element
                if let Some(tp) = elems
                    .iter()
                    .find(|e| matches!(type_table.get(**e), ResolvedType::TypePack { .. }))
                {
                    *tp
                } else {
                    // For TupleZip: use the first element type (all elements have the same shape)
                    elems[0]
                }
            } else {
                panic!("variadic for-of requires tuple iterable")
            }
        };

        // Resolve the body with the binding having the element type
        let (binding_name, binding_id, binding_name_span) = match &for_of.binding {
            crate::ast::Pattern::Ident { id, name, span } => (name.clone(), Some(*id), Some(*span)),
            crate::ast::Pattern::Tuple(..) => {
                // For destructuring patterns like [a, b], use a synthetic name
                // and resolve the destructuring in the body
                (format!("__pattern_temp_{unique_id}"), None, None)
            }
            _ => {
                panic!("variadic for-of does not support this binding pattern")
            }
        };

        let is_mut = for_of.is_mut;
        let is_destructured = matches!(&for_of.binding, crate::ast::Pattern::Tuple(..));

        ctx.enter_scope();
        let binding_local = ctx.add_local(binding_name.clone(), binding_type, is_mut, binding_id);
        if let (Some(id), Some(name_span)) = (binding_id, binding_name_span) {
            self.record_local_symbol(id, &binding_name, name_span, is_mut, binding_type);
        }

        // For destructured bindings (e.g., [a, b]), add the sub-bindings and
        // prepend destructuring assignments to the body.
        let mut destruct_stmts = Vec::new();
        if is_destructured && let crate::ast::Pattern::Tuple(tp, _) = &for_of.binding {
            let inner_elems = self
                .tysys
                .type_table
                .borrow()
                .as_tuple(binding_type)
                .unwrap_or_else(|| vec![binding_type]);
            for (i, pat_elem) in tp.iter().enumerate() {
                if let crate::ast::Pattern::Ident {
                    id,
                    name,
                    span: name_span,
                } = pat_elem
                {
                    let elem_type: TypeId =
                        inner_elems.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                    let local_idx = ctx.add_local(name.clone(), elem_type, is_mut, Some(*id));
                    self.record_local_symbol(*id, name, *name_span, is_mut, elem_type);
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

        let mut body_stmts = destruct_stmts;
        for stmt in &for_of.body.stmts {
            body_stmts.extend(self.resolve_stmt(stmt, ctx));
        }
        ctx.exit_scope();

        let body = TirBlock::new(body_stmts, span);

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

    fn resolve_tuple_for_of(
        &mut self,
        for_of: &ForOfStmt,
        iterable: TirExpr,
        elems: &[TypeId],
        is_enumerate: bool,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let span = for_of.span;

        // Validate: break, continue, and return are not allowed inside tuple for-of
        // because the loop is expanded at compile time into sequential blocks.
        if let Some((kind, bad_span)) = Self::find_control_flow_in_block(&for_of.body) {
            let _ = self.logger.error(TypeError::InvalidPattern {
                message: format!(
                    "`{kind}` is not allowed inside a tuple for-of loop (the loop is expanded at compile time)"
                ),
                span: bad_span,
            });
            return vec![TirStmt::new(TirStmtKind::Expr(iterable), span)];
        }
        let unique_id = ctx.next_local;

        // Store iterable in a temp variable to avoid re-evaluation
        let tuple_type_id = iterable.type_id;
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

        // Stage 5: when reify will consume the annotations, capture each
        // unrolled element's body facts separately. The body is a single
        // source sub-tree resolved once per element here; without
        // per-element capture every `AstId`-keyed map would be overwritten
        // so only the last element's facts survive (reify would then
        // dispatch every element to the last element's methods). Snapshot
        // the maps' pre-loop lengths; after each element, peel off and
        // truncate the freshly recorded tail. See `ElementOverlay`.
        let overlay_base = self
            .capture_tuple_overlays
            .then(|| self.sem.types.overlay_base_lens());
        let mut element_overlays: Vec<super::sem::types::ElementOverlay> = Vec::new();

        for (i, &elem_type) in elems.iter().enumerate() {
            ctx.enter_scope();

            // Create field access: __tuple_N.i
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
                // For enumerate: binding is typically [idx, val]
                // Create a synthetic tuple [i32_literal, element] and destructure
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

                // Resolve the binding pattern against this [i32, elem_type] tuple
                let tir_pattern = self.resolve_let_pattern(
                    &for_of.binding,
                    enum_tuple_type,
                    for_of.is_mut,
                    span,
                    ctx,
                );
                block_stmts.push(TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern: tir_pattern,
                        is_mut: for_of.is_mut,
                        value: enum_tuple,
                    },
                    span,
                ));
            } else {
                // Simple case: bind element to the pattern
                match &for_of.binding {
                    Pattern::Ident {
                        id,
                        name,
                        span: name_span,
                    }
                    | Pattern::MutIdent {
                        id,
                        name,
                        span: name_span,
                    } => {
                        let is_mut =
                            for_of.is_mut || matches!(&for_of.binding, Pattern::MutIdent { .. });
                        let local_index = ctx.add_local(name.clone(), elem_type, is_mut, Some(*id));
                        self.record_local_symbol(*id, name, *name_span, is_mut, elem_type);
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
                    Pattern::Tuple(_, _) | Pattern::Struct { .. } => {
                        let tir_pattern = self.resolve_let_pattern(
                            &for_of.binding,
                            elem_type,
                            for_of.is_mut,
                            span,
                            ctx,
                        );
                        block_stmts.push(TirStmt::new(
                            TirStmtKind::LetDestructure {
                                pattern: tir_pattern,
                                is_mut: for_of.is_mut,
                                value: field_access,
                            },
                            span,
                        ));
                    }
                    Pattern::Wildcard => {
                        // Discard the element
                        block_stmts.push(TirStmt::new(TirStmtKind::Expr(field_access), span));
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: "invalid binding pattern in for-of loop".to_string(),
                            span,
                        });
                    }
                }
            }

            // Resolve the body AST (each expansion gets its own resolution with different types)
            let body = self.resolve_block(&for_of.body, ctx, None);
            block_stmts.extend(body.stmts);

            // Capture this element's body annotations and reset the maps
            // back to their pre-loop state so the next element records from
            // a clean slate (Stage 5; reify-only).
            if let Some(base) = overlay_base {
                element_overlays.push(self.sem.types.split_off_overlay(base));
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

        // Record this for-of's per-element overlays as one instantiation
        // (in deterministic walk order). A nested inner for-of resolves
        // once per outer element, appending one entry per outer element;
        // reify's visit counter pairs them up in the same order.
        if overlay_base.is_some() {
            let for_of_key = self.ann_key(for_of.id);
            self.sem
                .types
                .tuple_overlays
                .entry(for_of_key)
                .or_default()
                .push(element_overlays);
        }

        // Wrap everything in a labeled block for break support
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

    /// Lower `for let v of iterable { body }` directly into TIR for non-tuple
    /// iterables. Produces:
    ///
    /// ```text
    /// __for_of_N: {
    ///     let mut __iter_N = iterable.into_iter();
    ///     loop {
    ///         match __iter_N.next() {
    ///             Option::Some(v) => body,
    ///             _ => break,
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// The synthetic `__iter_N` local is registered with `defining_ast_id:
    /// None` (same convention as `assert`'s `__cond` / `__vK` temps) so it
    /// never enters `local_symbols`. That keeps LSP hover / jump-to-def on
    /// the `for` keyword from surfacing the helper name. The `.into_iter()`
    /// / `.next()` dispatches go through [`Self::resolve_method_call_with`]
    /// with `method_id: None`, so no use→def edges are recorded against
    /// `for_of.id` either — clicking the `for` keyword no longer drags the
    /// user into `Iterator::next` in `core:prelude/list.wado`.
    ///
    /// `for_of.iterable` is resolved as-is — if the user wrote
    /// `for let item of expr.enumerate()`, the `.enumerate()` is part of
    /// the AST and flows naturally into the iterator chain. (The previous
    /// `is_enumerate` parameter survived only because `resolve_tuple_for_of`
    /// uses it to special-case the index binding; the iterator path has
    /// nothing to do with it. The pre-refactor implementation wrapped the
    /// already-enumerated AST in a second `.enumerate()`, producing
    /// `IterEnumerate<IterEnumerate<…>>` and ICE-ing at codegen with
    /// "unsubstituted `AssocTypeProjection` `Item` reached codegen" — see
    /// `tests/fixtures/for_of_iterator_enumerate.wado`.)
    fn resolve_iterator_for_of(
        &mut self,
        for_of: &ForOfStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use super::method_call::MethodCallInput;

        let span = for_of.span;
        let unique_id = ctx.next_local;
        let iter_var = format!("__iter_{unique_id}");
        let label = format!("__for_of_{unique_id}");

        // Resolve the iterable receiver verbatim, then dispatch `.into_iter()`
        // on it. Whatever adapter chain the user wrote (e.g. `.enumerate()`,
        // `.filter(…)`, `.map(…)`) is already part of `for_of.iterable`.
        let into_iter_receiver = self.resolve_expr(&for_of.iterable, ctx, None);

        // `<receiver>.into_iter()`
        let into_iter_call = self.resolve_method_call_with(
            MethodCallInput {
                receiver: into_iter_receiver,
                method_name: "into_iter",
                method_id: None,
                call_id: None,
                type_args: vec![],
                args: &[],
                expected_type: None,
                span,
            },
            ctx,
        );
        // Capture the `(self_kind, is_ref_impl)` the dispatch chose
        // (Gap 6 of WEP 2026-05-26): the synthetic call passed
        // `call_id == None` so `record_method_dispatch` skipped it, but
        // reify needs the receiver-adjustment inputs to reproduce the
        // same call shape. Also capture the resolved `FunctionRef`
        // before `into_iter_call` is consumed by the iterator `let`.
        let into_iter_dispatch = self.pending_method_dispatch.take();
        let into_iter_func = match &into_iter_call.kind {
            TirExprKind::MethodCall { func, .. } => Some(func.clone()),
            _ => None,
        };
        let iter_type = into_iter_call.type_id;

        // Iterator-trait conformance check, mirroring the pre-refactor
        // surface error.
        if !self.type_implements_trait(iter_type, "Iterator")
            && !matches!(
                self.tysys.type_table.borrow().get(iter_type),
                ResolvedType::Unknown | ResolvedType::TypeParam { .. }
            )
        {
            let type_name = self.tysys.type_table.borrow().type_name(iter_type);
            let _ = self.logger.error(TypeError::MissingTraitImpl {
                type_name,
                trait_name: "Iterator".to_string(),
                span,
            });
        }

        // `let mut __iter_N = …;` — `defining_ast_id: None` keeps this
        // synthetic local out of `local_symbols`.
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

        // Make `__for_of_N` visible to a body-level `break __for_of_N`
        // (no existing user does this, but the validation in `resolve_break`
        // would otherwise reject it). Pop after the body has been resolved.
        ctx.active_labels.push(label.clone());

        // `__iter_N.next()` — dispatch on the Local receiver, no AST.
        let iter_local_ref = TirExpr::new(
            TirExprKind::Local {
                index: iter_local_index,
                name: iter_var,
            },
            iter_type,
            span,
        );
        let next_call = self.resolve_method_call_with(
            MethodCallInput {
                receiver: iter_local_ref,
                method_name: "next",
                method_id: None,
                call_id: None,
                type_args: vec![],
                args: &[],
                expected_type: None,
                span,
            },
            ctx,
        );
        let next_dispatch = self.pending_method_dispatch.take();
        let next_func = match &next_call.kind {
            TirExprKind::MethodCall { func, .. } => Some(func.clone()),
            _ => None,
        };
        let option_type = next_call.type_id;

        // Build the `Option::Some(<user binding>)` arm pattern directly as
        // TIR. Resolving the user's `for_of.binding` against the Item type
        // delegates name binding / destructuring to `resolve_if_pattern_inner`,
        // which preserves the binding's real `AstId` (LSP hover on the loop
        // variable still works). The wrapping `TirPattern::Variant` is
        // built by hand so the lowering never synthesises an AST node — the
        // `Some` token has no source position, so giving it one would be
        // misleading.
        let some_case_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
            .to_string();
        // `.next()` returns `Option<Item>`. Extract the `Some` payload type
        // for the binding scrutinee. Bind out of the borrow first so the
        // `get_variant_case_payload_type` call below can re-borrow `&mut self`.
        let option_shape: Option<(String, Vec<TypeId>)> =
            match self.tysys.type_table.borrow().get(option_type).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } if self.contains_variant(&name) => Some((name, type_args)),
                ResolvedType::Variant { name, .. } if self.contains_variant(&name) => {
                    Some((name, vec![]))
                }
                _ => None,
            };
        let item_type = match option_shape {
            Some((name, type_args)) => {
                self.get_variant_case_payload_type(&name, &some_case_name, &type_args, span)
            }
            // `.next()` returned an unexpected non-Option type. The iterator-
            // trait check above (or method dispatch downstream) has already
            // diagnosed it; degrade to `UNKNOWN` to keep resolution going.
            None => TypeTable::UNKNOWN,
        };

        // Stage 5 (Gap 6 of WEP 2026-05-26): record the iterator-path
        // dispatch decision so reify can re-emit the synthetic
        // `into_iter()` / `next()` calls without re-dispatching. Only
        // record when both dispatches succeeded (the trait-check error
        // path above bailed without resolving them).
        if let (
            Some(into_iter_func),
            Some(into_iter_dispatch),
            Some(next_func),
            Some(next_dispatch),
        ) = (into_iter_func, into_iter_dispatch, next_func, next_dispatch)
        {
            self.record_for_of_iterator(
                for_of.id,
                super::sem::types::ForOfIteratorInfo {
                    into_iter: into_iter_func,
                    into_iter_self_kind: into_iter_dispatch.0,
                    into_iter_is_ref_impl: into_iter_dispatch.1,
                    next: next_func,
                    next_self_kind: next_dispatch.0,
                    next_is_ref_impl: next_dispatch.1,
                    item_type,
                    iter_type,
                },
            );
        }

        ctx.enter_scope();
        let binding_pattern =
            self.resolve_if_pattern_inner(&for_of.binding, item_type, ctx, span, RefBinding::None);
        let body_block = self.resolve_block(&for_of.body, ctx, None);
        ctx.exit_scope();

        let some_pattern = TirPattern::Variant {
            enum_type: option_type,
            variant_name: some_case_name,
            bindings: vec![binding_pattern],
            payload_type: item_type,
        };

        let body_type = Self::block_result_type(&body_block);
        let some_body = TirExpr::new(TirExprKind::Block(body_block), body_type, span);

        // `_ => break;` — Wildcard arm body is a block holding a single
        // `Break`, mirroring how `while let` lowers its fall-through arm
        // (see `resolve_while`'s `Condition::LetChain` path).
        let break_stmt = TirStmt::new(
            TirStmtKind::Break {
                label: None,
                value: None,
            },
            span,
        );
        let break_block = TirBlock::new(vec![break_stmt], span);
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

        let result = vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label,
                block: TirBlock::new(vec![iter_let, loop_tir], span),
            },
            span,
        )];
        ctx.active_labels.pop();
        result
    }

    /// Check if a block contains `break`, `continue`, or `return` at the top level
    /// (not inside nested loops/functions where they would be valid).
    /// Returns the kind name and span of the first offending statement.
    fn find_control_flow_in_block(block: &Block) -> Option<(&'static str, Span)> {
        for stmt in &block.stmts {
            if let Some(found) = Self::find_control_flow_in_stmt(stmt) {
                return Some(found);
            }
        }
        None
    }

    fn find_control_flow_in_stmt(stmt: &Stmt) -> Option<(&'static str, Span)> {
        match stmt {
            Stmt::Break(b) => Some(("break", b.span)),
            Stmt::Continue(c) => Some(("continue", c.span)),
            // return and task return are allowed — they exit the enclosing function,
            // which is well-defined even in compile-time-expanded blocks.
            Stmt::Return(_) | Stmt::TaskReturn(_) => None,
            // Recurse into blocks that don't introduce a new loop/function scope
            Stmt::If(if_stmt) => {
                if let Some(found) = Self::find_control_flow_in_block(&if_stmt.then_block) {
                    return Some(found);
                }
                if let Some(else_block) = &if_stmt.else_block {
                    return Self::find_control_flow_in_block(else_block);
                }
                None
            }
            Stmt::LabeledBlock(lb) => Self::find_control_flow_in_block(&lb.block),
            // Don't recurse into loops/closures — break/continue/return there are valid
            _ => None,
        }
    }

    /// Resolve a break statement
    pub(super) fn resolve_break(
        &mut self,
        break_stmt: &BreakStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        // Resolve the break value against the target block's expected type
        // so that literals coerce correctly (e.g. `break label: 10` when the
        // block is used as `let x: i64 = label: { ... }`).
        let expected = break_stmt.label.as_ref().and_then(|label| {
            ctx.labeled_block_targets
                .iter()
                .rev()
                .find(|t| &t.label == label)
                .and_then(|t| t.expected_type)
        });
        let value = break_stmt
            .value
            .as_ref()
            .map(|v| self.resolve_expr(v, ctx, expected));

        // Validate that the target label exists
        if let Some(label) = &break_stmt.label
            && !ctx.active_labels.iter().any(|l| l == label)
        {
            let _ = self.logger.error(TypeError::UnknownIdentifier {
                name: format!("labeled break target not found: {label}"),
                span: break_stmt.span,
            });
        }

        // If breaking with a value to a labeled block expression, record the
        // type. Scan innermost-first so a `break label` inside a nested block
        // that reuses the same label name is attributed to the inner target —
        // consistent with the `expected_type` lookup above and with WIR `br`
        // depth resolution.
        if let (Some(label), Some(val)) = (&break_stmt.label, &value) {
            for target in ctx.labeled_block_targets.iter_mut().rev() {
                if &target.label == label {
                    target.break_types.push(val.type_id);
                    break;
                }
            }
        }

        TirStmt::new(
            TirStmtKind::Break {
                label: break_stmt.label.clone(),
                value,
            },
            break_stmt.span,
        )
    }

    /// Resolve a continue statement.
    ///
    /// Inside a C-style `for` body the loop's `update` expression must run
    /// before the next iteration, so we re-target naked `continue` to break
    /// out of the labeled body block instead of jumping to the top of the
    /// loop. `ctx.for_continue_labels` holds the body labels stacked by
    /// [`Self::resolve_for`]; empty outside a for body, in which case
    /// continue lowers naturally.
    pub(super) fn resolve_continue(
        &mut self,
        continue_stmt: &ContinueStmt,
        ctx: &FunctionContext,
    ) -> TirStmt {
        if let Some(label) = ctx.for_continue_labels.last() {
            TirStmt::new(
                TirStmtKind::Break {
                    label: Some(label.clone()),
                    value: None,
                },
                continue_stmt.span,
            )
        } else {
            TirStmt::new(TirStmtKind::Continue, continue_stmt.span)
        }
    }

    /// Resolve a `while` or `while let` loop directly into TIR.
    ///
    /// `while cond { B }` lowers to:
    ///
    /// ```text
    /// loop {
    ///     if !cond { break; }
    ///     B
    /// }
    /// ```
    ///
    /// `while let pat = expr { B }` (and the let-chain variant) lowers to:
    ///
    /// ```text
    /// loop {
    ///     match expr { pat => B, _ => break; }
    /// }
    /// ```
    ///
    /// Naked `break` / `continue` inside `B` target the synthesised
    /// `loop`, which is the correct semantics — no label re-targeting is
    /// required (unlike C-style `for`).
    pub(super) fn resolve_while(
        &mut self,
        w: &WhileStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let span = w.span;
        // Naked `continue` inside this while's body targets *this* loop,
        // not an enclosing C-style `for` body label.
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);
        let stmts = match &w.condition {
            Condition::Expr(cond_expr) => {
                self.record_desugar(w.id, super::sem::types::DesugarKind::While);
                let cond_tir = self.resolve_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let cond_span = cond_expr.span();
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
                let body_block = self.resolve_block(&w.body, ctx, None);
                let mut stmts = Vec::with_capacity(1 + body_block.stmts.len());
                stmts.push(if_break);
                stmts.extend(body_block.stmts);
                stmts
            }
            Condition::LetChain {
                elements,
                span: cond_span,
            } => {
                self.record_desugar(w.id, super::sem::types::DesugarKind::WhileLetChain);
                // The else-branch unconditionally terminates the loop. Build it as a
                // TirBlock holding a single `Break` so `resolve_let_chain_stmts`
                // (shared with `if let` resolution) can splice it in as the
                // wildcard arm of the synthesised match.
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let else_block = TirBlock::new(vec![break_stmt], *cond_span);
                ctx.enter_scope();
                let body_stmts = self.resolve_let_chain_stmts(
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

    /// Resolve a C-style `for init; cond; update { B }` loop directly into TIR.
    ///
    /// Lowered shape:
    ///
    /// ```text
    /// {
    ///     init;                        // when present
    ///     loop {
    ///         if !cond { break; }      // omitted when `cond` is absent
    ///         __for_N_body: { B }
    ///         update;                  // when present
    ///     }
    /// }
    /// ```
    ///
    /// The outer `{ … }` is a fresh elaborator scope so `init`'s bindings stay
    /// local to the for. `B` is wrapped in `__for_N_body` so that naked
    /// `continue` (which would otherwise skip the `update`) is rerouted via
    /// [`Self::resolve_continue`] to `break __for_N_body`, letting control
    /// fall through to `update;` before the next iteration. Naked `break`
    /// already targets the innermost loop, which is the `loop {}` here, so
    /// no extra rewriting is needed.
    ///
    /// `while let` form `for init; let pat = e; update { B }` lowers to:
    ///
    /// ```text
    /// {
    ///     init;
    ///     loop {
    ///         match e {
    ///             pat => {
    ///                 __for_N_body: { B }
    ///                 update;
    ///             }
    ///             _ => break;
    ///         }
    ///     }
    /// }
    /// ```
    pub(super) fn resolve_for(&mut self, f: &ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        self.record_desugar(f.id, super::sem::types::DesugarKind::CStyleFor);
        let span = f.span;
        let loop_id = ctx.next_loop_id;
        ctx.next_loop_id += 1;
        let body_label = format!("__for_{loop_id}_body");

        // Mirror `resolve_loop` / `resolve_while` / `resolve_for_of`: clear the
        // continue-retarget stack at the loop boundary so the invariant
        // ("the stack lists labels for the enclosing C-style `for` bodies that
        // a naked `continue` should `break` to, innermost-first") cannot be
        // violated by a future refactor that resolves part of the body
        // outside `resolve_for_labeled_body`. Today the push/pop inside
        // that helper alone would suffice, but the symmetry guards against
        // a body-resolution path moving above the helper.
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);

        // The outer scope holds `init`'s bindings so the loop body can see
        // them while the surrounding function cannot.
        ctx.enter_scope();

        let mut outer_stmts: Vec<TirStmt> = Vec::new();
        if let Some(init) = &f.init {
            outer_stmts.extend(self.resolve_stmt(init, ctx));
        }

        // Build the per-iteration sequence. Body and update are resolved
        // here (rather than up-front) because in the let-chain form the
        // pattern's bindings must be in scope for both — see
        // `lib/core/prelude/string.wado::String::find_char` for a real
        // case where `update` references a pattern-bound name.
        let iter_stmts: Vec<TirStmt> = match &f.condition {
            None => {
                let labeled_body = self.resolve_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![labeled_body];
                s.extend(self.resolve_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(Condition::Expr(cond_expr)) => {
                let cond_tir = self.resolve_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let cond_span = cond_expr.span();
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
                let labeled_body = self.resolve_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![if_break, labeled_body];
                s.extend(self.resolve_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(Condition::LetChain {
                elements,
                span: cond_span,
            }) => {
                // The parser only accepts a single Let element in a for
                // header — multi-element let-chains are syntactically
                // limited to `if`/`while`. Future grammar evolution that
                // relaxes this should surface here as a diagnostic, not
                // an ICE.
                let single_let = if elements.len() == 1 {
                    match &elements[0] {
                        ConditionElement::Let {
                            pattern,
                            expr,
                            span: elem_span,
                        } => Some((pattern, expr, *elem_span)),
                        ConditionElement::Expr(_) => None,
                    }
                } else {
                    None
                };
                let Some((pattern, expr, elem_span)) = single_let else {
                    let _ = self.logger.error(TypeError::InvalidPattern {
                        message: "for-header let-chain must consist of a single \
                                  `let pattern = expr` element"
                            .to_string(),
                        span: *cond_span,
                    });
                    ctx.exit_scope();
                    ctx.for_continue_labels = saved_continue;
                    return vec![];
                };

                let scrutinee = self.resolve_expr(expr, ctx, None);
                let scrutinee_type = scrutinee.type_id;
                ctx.enter_scope();
                let tir_pattern = self.resolve_if_pattern(pattern, scrutinee_type, ctx, elem_span);
                // Body and update both run inside the pattern scope so
                // they can name the bindings introduced by `pat`.
                let labeled_body = self.resolve_for_labeled_body(&body_label, &f.body, ctx);
                let update_stmts = self.resolve_for_update(f.update.as_ref(), ctx);
                ctx.exit_scope();

                let mut then_stmts = vec![labeled_body];
                then_stmts.extend(update_stmts);
                // The match sits in statement position, so its overall type
                // is Unit. The then-block ends without a value-producing
                // trailing expression (labeled_body is a stmt and update
                // is a Unit-typed Expr-stmt), so its block_result_type is
                // Unit. The else-block holds only a `Break`, which makes
                // it Never; mark it as such rather than lying with Unit
                // so any downstream analysis that consults arm body
                // types sees the truth.
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

    /// Resolve a for loop's body wrapped in its continue-retarget label.
    /// Pushes `body_label` onto both `for_continue_labels` (so naked
    /// `continue` inside the body becomes `break <body_label>`) and
    /// `active_labels` (so the label validates as a known break target).
    fn resolve_for_labeled_body(
        &mut self,
        body_label: &str,
        body: &Block,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        ctx.for_continue_labels.push(body_label.to_string());
        ctx.active_labels.push(body_label.to_string());
        let body_block = self.resolve_block(body, ctx, None);
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

    /// Resolve a for loop's optional update expression as a single
    /// statement, or no statements if absent.
    fn resolve_for_update(
        &mut self,
        update: Option<&Expr>,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        update
            .map(|u| {
                let tir = self.resolve_expr(u, ctx, None);
                vec![TirStmt::new(TirStmtKind::Expr(tir), u.span())]
            })
            .unwrap_or_default()
    }
}

fn format_pattern_qualifier_type(ty: &Type) -> String {
    match ty {
        Type::Named(t) => t.name.clone(),
        Type::Generic(t) => {
            let args = t
                .args
                .iter()
                .map(format_pattern_qualifier_type)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}<{args}>", t.name)
        }
        Type::NamespacedGeneric(t) => {
            if t.args.is_empty() {
                format!("{}::{}", t.namespace, t.name)
            } else {
                let args = t
                    .args
                    .iter()
                    .map(format_pattern_qualifier_type)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}::{}<{args}>", t.namespace, t.name)
            }
        }
        Type::Function(_) => "fn".to_string(),
        Type::Tuple(types) => {
            let elems = types
                .iter()
                .map(format_pattern_qualifier_type)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{elems}]")
        }
        Type::Reference(inner) => format!("&{}", format_pattern_qualifier_type(inner)),
        Type::MutReference(inner) => format!("&mut {}", format_pattern_qualifier_type(inner)),
        Type::TypePackSpread(name, _) => format!("..{name}"),
        Type::Error(_) => "<error>".to_string(),
    }
}

/// Collect `(binding_name -> AstId)` from an AST `Pattern`. Used by the
/// or-pattern handler to align every alternative's `defining_ast_id` with
/// the first alternative's source node, so that LSP jump-to-def from a use
/// in the arm body lands on the first alternative's binding.
fn collect_ast_pattern_binding_ids(
    pattern: &Pattern,
    out: &mut crate::hashmap::IndexMap<String, AstId>,
) {
    match pattern {
        Pattern::Ident { id, name, .. } | Pattern::MutIdent { id, name, .. } => {
            out.entry(name.clone()).or_insert(*id);
        }
        Pattern::Tuple(patterns, _) => {
            for p in patterns {
                collect_ast_pattern_binding_ids(p, out);
            }
        }
        Pattern::Variant { bindings, .. } => {
            for p in bindings {
                collect_ast_pattern_binding_ids(p, out);
            }
        }
        Pattern::Struct { fields, .. } => {
            for f in fields {
                collect_ast_pattern_binding_ids(&f.pattern, out);
            }
        }
        Pattern::Or(alternatives) => {
            if let Some(first) = alternatives.first() {
                collect_ast_pattern_binding_ids(first, out);
            }
        }
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Error(_) => {}
    }
}

/// Collect binding names, local indices, and types from a TIR pattern for or-pattern validation.
pub(super) fn collect_pattern_bindings_with_index(
    pattern: &TirPattern,
) -> Vec<(String, u32, crate::tir::TypeId)> {
    let mut bindings = Vec::new();
    collect_pattern_bindings_with_index_inner(pattern, &mut bindings);
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    bindings
}

fn collect_pattern_bindings_with_index_inner(
    pattern: &TirPattern,
    out: &mut Vec<(String, u32, crate::tir::TypeId)>,
) {
    match pattern {
        TirPattern::Binding {
            name,
            local_index,
            type_id,
        } => {
            out.push((name.clone(), *local_index, *type_id));
        }
        TirPattern::Tuple(patterns, _) => {
            for p in patterns {
                collect_pattern_bindings_with_index_inner(p, out);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                collect_pattern_bindings_with_index_inner(p, out);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                collect_pattern_bindings_with_index_inner(&f.pattern, out);
            }
        }
        TirPattern::Or(alternatives) => {
            if let Some(first) = alternatives.first() {
                collect_pattern_bindings_with_index_inner(first, out);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
    }
}

/// Remap a specific `local_index` in a pattern to a new value.
pub(super) fn remap_pattern_local(pattern: &mut TirPattern, from: u32, to: u32) {
    match pattern {
        TirPattern::Binding { local_index, .. } => {
            if *local_index == from {
                *local_index = to;
            }
        }
        TirPattern::Tuple(patterns, _) => {
            for p in patterns {
                remap_pattern_local(p, from, to);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                remap_pattern_local(p, from, to);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                remap_pattern_local(&mut f.pattern, from, to);
            }
        }
        TirPattern::Or(alternatives) => {
            for p in alternatives {
                remap_pattern_local(p, from, to);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
    }
}

/// Resolve a primitive type's builtin associated constant (`i32::MIN`,
/// `u8::MAX`, …) to its `i128` value. Pure and `self`-free so both the
/// elaborator's pattern lowering and the reify pass share one source of
/// truth for the range-endpoint / const-pattern paths. Returns `None`
/// for non-primitive qualifiers or unknown const names.
pub(super) fn primitive_assoc_const_to_i128(
    qualifier: Option<&Type>,
    const_name: &str,
) -> Option<i128> {
    let ty_name = match qualifier? {
        Type::Named(named) => named.name.as_str(),
        Type::Generic(generic) => generic.name.as_str(),
        Type::NamespacedGeneric(namespaced) => namespaced.name.as_str(),
        Type::Function(_)
        | Type::Tuple(_)
        | Type::Reference(_)
        | Type::MutReference(_)
        | Type::TypePackSpread(_, _)
        | Type::Error(_) => return None,
    };
    match (ty_name, const_name) {
        ("i8", "MAX") => Some(i128::from(i8::MAX)),
        ("i8", "MIN") => Some(i128::from(i8::MIN)),
        ("i16", "MAX") => Some(i128::from(i16::MAX)),
        ("i16", "MIN") => Some(i128::from(i16::MIN)),
        ("i32", "MAX") => Some(i128::from(i32::MAX)),
        ("i32", "MIN") => Some(i128::from(i32::MIN)),
        ("i64", "MAX") => Some(i128::from(i64::MAX)),
        ("i64", "MIN") => Some(i128::from(i64::MIN)),
        ("u8", "MAX") => Some(i128::from(u8::MAX)),
        ("u8", "MIN") => Some(i128::from(u8::MIN)),
        ("u16", "MAX") => Some(i128::from(u16::MAX)),
        ("u16", "MIN") => Some(i128::from(u16::MIN)),
        ("u32", "MAX") => Some(i128::from(u32::MAX)),
        ("u32", "MIN") => Some(i128::from(u32::MIN)),
        ("u64", "MAX") => Some(i128::from(u64::MAX)),
        ("u64", "MIN") => Some(i128::from(u64::MIN)),
        _ => None,
    }
}
