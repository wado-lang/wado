//! Statement resolution (let, return, if, loop, break, continue, etc.).

use crate::ast::{
    self, Block, BreakStmt, ContinueStmt, ExprStmt, IfStmt, LetStmt, Literal, LoopStmt, Pattern,
    ReturnStmt, Stmt, TaskReturnStmt,
};
use crate::compiler_host::CompilerHost;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirLiteralPattern, TirPattern,
    TirStmt, TirStmtKind, TirStructField, TirStructPatternField, TypeId, TypeTable,
};
use crate::token::Span;

use super::Resolver;
use super::types::{FunctionContext, TypeError};
use super::util;

impl<H: CompilerHost> Resolver<'_, H> {
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
            // Propagate expected type to the last expression for coercion
            if expected_type.is_some()
                && i == len - 1
                && let Stmt::Expr(expr_stmt) = s
            {
                let expr = self.resolve_expr(&expr_stmt.expr, ctx, expected_type);
                stmts.push(TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span));
                continue;
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
            // While, For, ForOf are desugared to Loop in the desugar phase
            Stmt::While(_) => unreachable!("While should be desugared before resolving"),
            Stmt::For(_) => unreachable!("For should be desugared before resolving"),
            Stmt::ForOf(_) => unreachable!("ForOf should be desugared before resolving"),
            Stmt::Loop(loop_stmt) => vec![self.resolve_loop(loop_stmt, ctx)],
            Stmt::Break(break_stmt) => vec![self.resolve_break(break_stmt, ctx)],
            Stmt::Continue(continue_stmt) => vec![self.resolve_continue(continue_stmt)],
            Stmt::Assert(_) => {
                // Assert statements are desugared in the desugar phase before resolution
                panic!("Assert should be desugared before resolving");
            }
            Stmt::LabeledBlock(labeled_block) => {
                vec![self.resolve_labeled_block(labeled_block, ctx)]
            }
        }
    }

    /// Resolve a labeled block statement
    pub(super) fn resolve_labeled_block(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        ctx.active_labels.push(labeled_block.label.clone());
        // resolve_block already handles scope entry/exit
        let block = self.resolve_block(&labeled_block.block, ctx, None);
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
        // Check for tuple literal to array coercion when type annotation is present
        let (value, type_id) = if let Some(annotated_type) = &let_stmt.ty {
            let target_type = self.resolve_type(annotated_type);

            // Special case: tuple literal with Array<T> or Tuple type annotation
            if let ast::Expr::TupleLiteral(tuple_lit) = &let_stmt.value {
                // let a: Array<i32> = [1, 2, 3]
                let element_type_opt = self.type_table.borrow().as_array(target_type);
                if let Some(element_type) = element_type_opt {
                    let elements: Vec<TirExpr> = tuple_lit
                        .elements
                        .iter()
                        .map(|elem| {
                            let resolved = self.resolve_expr(elem, ctx, Some(element_type));
                            if resolved.type_id != element_type
                                && resolved.type_id != TypeTable::UNKNOWN
                                && resolved.type_id != TypeTable::NEVER
                            {
                                let _ = self.logger.error(TypeError::TypeMismatch {
                                    expected: self.type_table.borrow().type_name(element_type),
                                    found: self.type_table.borrow().type_name(resolved.type_id),
                                    span: elem.span(),
                                });
                            }
                            resolved
                        })
                        .collect();

                    let value = TirExpr::new(
                        TirExprKind::ArrayLiteral { elements },
                        target_type,
                        let_stmt.value.span(),
                    );
                    (value, target_type)
                } else {
                    let target_resolved = self.type_table.borrow().get(target_type).clone();
                    if let ResolvedType::Tuple(expected_elem_types) = target_resolved {
                        // let t: [i32, String] = [1, "hello"] - check element types
                        let expected_elem_types = expected_elem_types.clone();
                        let elements: Vec<TirExpr> = tuple_lit
                            .elements
                            .iter()
                            .enumerate()
                            .map(|(i, elem)| {
                                let expected = expected_elem_types.get(i).copied();
                                let resolved = self.resolve_expr(elem, ctx, expected);
                                // Check if element type matches expected
                                if let Some(expected_type) = expected
                                    && resolved.type_id != expected_type
                                    && resolved.type_id != TypeTable::UNKNOWN
                                {
                                    let _ = self.logger.error(TypeError::TypeMismatch {
                                        expected: self.type_table.borrow().type_name(expected_type),
                                        found: self.type_table.borrow().type_name(resolved.type_id),
                                        span: elem.span(),
                                    });
                                }
                                resolved
                            })
                            .collect();

                        // Also check length mismatch
                        if tuple_lit.elements.len() != expected_elem_types.len() {
                            let _ = self.logger.error(TypeError::TypeMismatch {
                                expected: format!(
                                    "tuple with {} elements",
                                    expected_elem_types.len()
                                ),
                                found: format!("tuple with {} elements", tuple_lit.elements.len()),
                                span: let_stmt.value.span(),
                            });
                        }

                        let value = TirExpr::new(
                            TirExprKind::TupleLiteral { elements },
                            target_type,
                            let_stmt.value.span(),
                        );
                        (value, target_type)
                    } else {
                        let value = self.resolve_expr(&let_stmt.value, ctx, Some(target_type));
                        (value, target_type)
                    }
                }
            } else if let ast::Expr::StructLiteral(struct_lit) = &let_stmt.value {
                // Handle implicit struct literal: let p: Point = { x: 1, y: 2 }
                if struct_lit.name.is_none() {
                    // Check if target type is a struct
                    let target_resolved = self.type_table.borrow().get(target_type).clone();
                    if let ResolvedType::Struct { name, .. } = target_resolved {
                        let name = name.clone();
                        let struct_type = target_type;

                        let struct_field_types: Vec<(String, TypeId)> = self
                            .struct_fields
                            .get(&name)
                            .map(|info| info.fields.clone())
                            .unwrap_or_default();

                        let fields: Vec<TirStructField> = struct_lit
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(index, field)| {
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
                        self.try_coerce_struct_to_map(&let_stmt.value, ctx, target_type)
                    {
                        (coerced, target_type)
                    } else {
                        // Target type is not a struct or TreeMap - error
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: self.type_table.borrow().type_name(target_type),
                            found: "implicit struct literal".into(),
                            span: struct_lit.span,
                        });
                        let value = self.resolve_expr(&let_stmt.value, ctx, None);
                        (value, target_type)
                    }
                } else {
                    // Named struct literal - resolve normally
                    let value = self.resolve_expr(&let_stmt.value, ctx, Some(target_type));
                    (value, target_type)
                }
            } else {
                // Use expected type for numeric literal coercion
                let value = self.resolve_expr(&let_stmt.value, ctx, Some(target_type));
                (value, target_type)
            }
        } else {
            let value = self.resolve_expr(&let_stmt.value, ctx, None);
            (value.clone(), value.type_id)
        };

        // Type check: if type annotation is present, verify value type matches.
        // `never` (bottom type) is assignable to any type, so skip the check for it.
        if let Some(_annotated_type) = &let_stmt.ty
            && value.type_id != type_id
            && value.type_id != TypeTable::UNKNOWN
            && value.type_id != TypeTable::NEVER
        {
            // Allow null (Option<unknown>) to be assigned to Option<T>
            let is_null_to_option = {
                let type_table = self.type_table.borrow();
                type_table
                    .as_option(value.type_id)
                    .is_some_and(|inner| inner == TypeTable::UNKNOWN)
                    && type_table.as_option(type_id).is_some()
            };
            if !is_null_to_option {
                let _ = self.logger.error(TypeError::TypeMismatch {
                    expected: self.type_table.borrow().type_name(type_id),
                    found: self.type_table.borrow().type_name(value.type_id),
                    span: let_stmt.value.span(),
                });
            }
        }

        // Handle different pattern types
        match &let_stmt.pattern {
            ast::Pattern::Ident(name) => {
                let local_index = ctx.add_local(name.clone(), type_id, let_stmt.is_mut);
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: let_stmt.is_mut,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Tuple(_) => {
                // Tuple destructuring: let [a, b] = tuple_expr;
                let tir_pattern = self.resolve_let_pattern(
                    &let_stmt.pattern,
                    type_id,
                    let_stmt.is_mut,
                    let_stmt.span,
                    ctx,
                );
                TirStmt::new(
                    TirStmtKind::LetPattern {
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
                    TirStmtKind::LetPattern {
                        pattern: tir_pattern,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Wildcard => {
                // Wildcard pattern: let _ = expr; - evaluate but don't bind
                // We still need a local to store the value temporarily
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
            ast::Pattern::Literal(_) | ast::Pattern::Variant { .. } => {
                // These patterns are not valid in let statements
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "literal and variant patterns are not allowed in let statements"
                        .to_string(),
                    span: let_stmt.span,
                });
                // Return a dummy statement
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
        }
    }

    /// Resolve a let pattern (for tuple destructuring)
    pub(super) fn resolve_let_pattern(
        &mut self,
        pattern: &ast::Pattern,
        type_id: TypeId,
        is_mut: bool,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Ident(name) => {
                let local_index = ctx.add_local(name.clone(), type_id, is_mut);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id,
                }
            }
            ast::Pattern::Tuple(patterns) => {
                // Get element types from the tuple type
                let elem_types = {
                    let type_table = self.type_table.borrow();
                    if let ResolvedType::Tuple(elem_types) = type_table.get(type_id) {
                        elem_types.clone()
                    } else {
                        // Error: expected tuple type
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: "tuple type".to_string(),
                            found: type_table.type_name(type_id),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    }
                };

                // Check length
                if patterns.len() != elem_types.len() {
                    let _ = self.logger.error(TypeError::TypeMismatch {
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
                        self.resolve_let_pattern(p, elem_type, is_mut, span, ctx)
                    })
                    .collect();

                TirPattern::Tuple(tir_patterns)
            }
            ast::Pattern::Struct {
                type_name,
                fields,
                has_rest,
                span: pat_span,
            } => {
                // Get struct name from type
                let struct_name = {
                    let type_table = self.type_table.borrow();
                    match type_table.get(type_id) {
                        ResolvedType::Struct { name, .. } => Some(name.clone()),
                        _ => None,
                    }
                };

                // If named pattern, verify the type matches
                if let Some(expected_name) = type_name
                    && let Some(actual_name) = &struct_name
                {
                    // Compare the short name (strip module prefix if needed)
                    let actual_short = actual_name.rsplit("::").next().unwrap_or(actual_name);
                    if actual_short != expected_name {
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: expected_name.clone(),
                            found: self.type_table.borrow().type_name(type_id),
                            span: *pat_span,
                        });
                    }
                }

                if struct_name.is_none() {
                    let _ = self.logger.error(TypeError::TypeMismatch {
                        expected: "struct type".to_string(),
                        found: self.type_table.borrow().type_name(type_id),
                        span: *pat_span,
                    });
                    return TirPattern::Wildcard;
                }

                // Resolve each field pattern
                let mut tir_fields = Vec::new();
                for field in fields {
                    let (field_index, field_type) =
                        self.lookup_field_type(type_id, &field.field_name, field.span);
                    let sub_pattern = self.resolve_let_pattern(
                        &field.pattern,
                        field_type,
                        is_mut,
                        field.span,
                        ctx,
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
                    && let Some(struct_info) = self.struct_fields.get(sname)
                {
                    let total_fields = struct_info.fields.len();
                    if fields.len() != total_fields {
                        let missing: Vec<_> = struct_info
                            .fields
                            .iter()
                            .filter(|(name, _)| !fields.iter().any(|f| f.field_name == *name))
                            .map(|(name, _)| name.clone())
                            .collect();
                        if !missing.is_empty() {
                            let _ = self.logger.error(TypeError::TypeMismatch {
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
                // These patterns are not valid in let statements
                let _ = self.logger.error(TypeError::InvalidPattern {
                    message: "literal and variant patterns are not allowed in let statements"
                        .to_string(),
                    span,
                });
                TirPattern::Wildcard
            }
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
        let mut result = Vec::new();

        // Handle optional init binding (scoped to this if statement)
        if if_stmt.init.is_some() {
            ctx.enter_scope();
        }

        if let Some(init) = &if_stmt.init {
            result.push(self.resolve_let(init, ctx));
        }

        match &if_stmt.condition {
            ast::Condition::Expr(expr) => {
                // Regular expression condition
                let condition = self.resolve_expr(expr, ctx, Some(TypeTable::BOOL));
                let then_block = self.resolve_block(&if_stmt.then_block, ctx, None);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx, None));

                result.push(TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    },
                    if_stmt.span,
                ));
            }
            ast::Condition::Pattern { pattern, expr, .. } => {
                // Pattern match condition: if let Some(x) = expr { ... }
                let scrutinee = self.resolve_expr(expr, ctx, None);
                let scrutinee_type = scrutinee.type_id;

                // Enter scope for pattern bindings (they're only visible in then_block)
                ctx.enter_scope();

                // Resolve the pattern with type information from scrutinee
                let tir_pattern =
                    self.resolve_if_pattern(pattern, scrutinee_type, ctx, if_stmt.span);

                let then_block = self.resolve_block(&if_stmt.then_block, ctx, None);

                // Exit pattern binding scope before resolving else block
                ctx.exit_scope();

                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx, None));

                result.push(TirStmt::new(
                    TirStmtKind::IfPattern {
                        scrutinee,
                        pattern: tir_pattern,
                        then_block,
                        else_block,
                    },
                    if_stmt.span,
                ));
            }
        }

        if if_stmt.init.is_some() {
            ctx.exit_scope();
        }

        result
    }

    /// Resolve a pattern in an if-pattern context with type information from the scrutinee
    pub(super) fn resolve_if_pattern(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirPattern {
        match pattern {
            Pattern::Wildcard => TirPattern::Wildcard,
            Pattern::Ident(name) => {
                // The binding gets the scrutinee type (or inner type for Option patterns)
                let index = ctx.add_local(name.clone(), scrutinee_type, false);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index: index,
                    type_id: scrutinee_type,
                }
            }
            Pattern::Literal(lit) => {
                let tir_lit = match lit {
                    Literal::Number(n) => {
                        // Float literals cannot be used in match patterns
                        if util::is_float_only_literal(&n.repr) {
                            let _ = self.logger.error(TypeError::InvalidPattern {
                                message: "float literals cannot be used in match patterns"
                                    .to_string(),
                                span,
                            });
                            return TirPattern::Wildcard;
                        }
                        // Check if scrutinee type is unsigned
                        let scrutinee_resolved =
                            self.type_table.borrow().get(scrutinee_type).clone();
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
                            match util::parse_u128_literal(&n.repr) {
                                Ok(value) => TirLiteralPattern::U128(value),
                                Err(_) => TirLiteralPattern::U128(0),
                            }
                        } else {
                            match util::parse_i128_literal(&n.repr) {
                                Ok(value) => TirLiteralPattern::I128(value),
                                Err(_) => TirLiteralPattern::I128(0),
                            }
                        }
                    }
                    Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    Literal::Char(c) => TirLiteralPattern::Char(*c),
                    Literal::String(s) => TirLiteralPattern::String(s.value.clone()),
                    Literal::Null => TirLiteralPattern::Null,
                    _ => TirLiteralPattern::Null,
                };
                TirPattern::Literal(tir_lit)
            }
            Pattern::Tuple(patterns) => {
                // For tuple patterns, extract element types
                let element_types = if let ResolvedType::Tuple(types) =
                    self.type_table.borrow().get(scrutinee_type).clone()
                {
                    types
                } else {
                    vec![TypeTable::UNKNOWN; patterns.len()]
                };

                let resolved: Vec<TirPattern> = patterns
                    .iter()
                    .zip(
                        element_types
                            .iter()
                            .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                    )
                    .map(|(p, &ty)| self.resolve_if_pattern(p, ty, ctx, span))
                    .collect();
                TirPattern::Tuple(resolved)
            }
            Pattern::Variant {
                variant_name,
                bindings,
                span,
            } => {
                let resolved_type = self.type_table.borrow().get(scrutinee_type).clone();

                // Handle enum types (no payload, just discriminant matching)
                if let ResolvedType::Enum { name, .. } = &resolved_type {
                    if !bindings.is_empty() {
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!("enum case `{variant_name}` does not have a payload"),
                            span: *span,
                        });
                    }
                    // Look up the enum case index
                    if let Some(enum_info) = self.enum_cases.get(name) {
                        if let Some(case_data) =
                            enum_info.cases.iter().find(|c| c.name == *variant_name)
                        {
                            return TirPattern::Enum {
                                enum_type: scrutinee_type,
                                case_name: variant_name.clone(),
                                case_index: case_data.index,
                            };
                        }
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: format!(
                                "one of: {}",
                                enum_info
                                    .cases
                                    .iter()
                                    .map(|c| c.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            found: variant_name.clone(),
                            span: *span,
                        });
                        return TirPattern::Wildcard;
                    }
                    let _ = self.logger.error(TypeError::TypeMismatch {
                        expected: format!("enum type `{name}`"),
                        found: "unknown enum".to_string(),
                        span: *span,
                    });
                    return TirPattern::Wildcard;
                }

                // Each variant case has exactly one payload type.
                // Determine the payload type for the variant case.
                let payload_type: TypeId = match &resolved_type {
                    // Non-generic variant
                    ResolvedType::Variant { name, .. } => {
                        self.get_variant_case_payload_type(name, variant_name, &[], *span)
                    }
                    // Generic variant instantiation
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        // Check if this is a variant (not a struct)
                        if self.variant_cases.contains_key(name) {
                            self.get_variant_case_payload_type(name, variant_name, type_args, *span)
                        } else {
                            let _ = self.logger.error(TypeError::TypeMismatch {
                                expected: "variant type".to_string(),
                                found: name.clone(),
                                span: *span,
                            });
                            TypeTable::UNKNOWN
                        }
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::TypeMismatch {
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
                    vec![self.resolve_if_pattern(&bindings[0], payload_type, ctx, *span)]
                } else if bindings.is_empty() {
                    // Unit case like `None` - no bindings
                    vec![]
                } else {
                    // Multiple bindings are deprecated with single payload design.
                    // Error will be caught by test fixture updates.
                    bindings
                        .iter()
                        .map(|p| self.resolve_if_pattern(p, TypeTable::UNKNOWN, ctx, *span))
                        .collect()
                };

                TirPattern::Variant {
                    enum_type: scrutinee_type,
                    variant_name: variant_name.clone(),
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
                    let resolved = self.type_table.borrow().get(scrutinee_type).clone();
                    if let ResolvedType::Struct { ref name, .. } = resolved {
                        let actual_short = name.rsplit("::").next().unwrap_or(name);
                        if actual_short != expected_name {
                            let _ = self.logger.error(TypeError::TypeMismatch {
                                expected: expected_name.clone(),
                                found: self.type_table.borrow().type_name(scrutinee_type),
                                span: *pat_span,
                            });
                        }
                    }
                }

                let mut tir_fields = Vec::new();
                for field in fields {
                    let (field_index, field_type) =
                        self.lookup_field_type(scrutinee_type, &field.field_name, field.span);
                    let sub_pattern =
                        self.resolve_if_pattern(&field.pattern, field_type, ctx, field.span);
                    tir_fields.push(TirStructPatternField {
                        field_name: field.field_name.clone(),
                        field_index,
                        pattern: sub_pattern,
                    });
                }

                // Exhaustiveness check
                if !has_rest {
                    let struct_name = {
                        let type_table = self.type_table.borrow();
                        match type_table.get(scrutinee_type) {
                            ResolvedType::Struct { name, .. } => Some(name.clone()),
                            _ => None,
                        }
                    };
                    if let Some(ref sname) = struct_name
                        && let Some(struct_info) = self.struct_fields.get(sname)
                    {
                        let total_fields = struct_info.fields.len();
                        if fields.len() != total_fields {
                            let missing: Vec<_> = struct_info
                                .fields
                                .iter()
                                .filter(|(name, _)| !fields.iter().any(|f| f.field_name == *name))
                                .map(|(name, _)| name.clone())
                                .collect();
                            if !missing.is_empty() {
                                let _ = self.logger.error(TypeError::TypeMismatch {
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
        }
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
        let payload_opt = self.variant_cases.get(variant_name).and_then(|info| {
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
        if self.variant_cases.contains_key(variant_name) {
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: format!("valid case of variant {variant_name}"),
                found: case_name.to_string(),
                span,
            });
        } else {
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: "known variant type".to_string(),
                found: variant_name.to_string(),
                span,
            });
        }
        TypeTable::UNKNOWN
    }
    /// Resolve a loop statement (infinite loop)
    pub(super) fn resolve_loop(
        &mut self,
        loop_stmt: &LoopStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let body = self.resolve_block(&loop_stmt.body, ctx, None);
        TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)
    }

    /// Resolve a break statement
    pub(super) fn resolve_break(
        &mut self,
        break_stmt: &BreakStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let value = break_stmt
            .value
            .as_ref()
            .map(|v| self.resolve_expr(v, ctx, None));

        // Validate that the target label exists
        if let Some(label) = &break_stmt.label
            && !ctx.active_labels.iter().any(|l| l == label)
        {
            let _ = self.logger.error(TypeError::UnknownIdentifier {
                name: format!("labeled break target not found: {label}"),
                span: break_stmt.span,
            });
        }

        // If breaking with a value to a labeled block expression, record the type
        if let (Some(label), Some(val)) = (&break_stmt.label, &value) {
            // Find the labeled block target with this label
            for target in &mut ctx.labeled_block_targets {
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

    /// Resolve a continue statement
    pub(super) fn resolve_continue(&mut self, continue_stmt: &ContinueStmt) -> TirStmt {
        TirStmt::new(TirStmtKind::Continue, continue_stmt.span)
    }
}
