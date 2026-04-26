use std::cell::RefCell;
use std::rc::Rc;

use crate::name::{LocalMethodName, ModuleSource};
use crate::tir::FunctionRef;
use crate::tir::{
    CallArg, ResolvedType, TirBlock, TirExpr, TirExprKind, TirLiteralPattern, TirModule,
    TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Helper to wrap an expression in a block (for if-else branches)
fn expr_to_block(expr: &TirExpr, span: Span) -> TirBlock {
    TirBlock {
        stmts: vec![TirStmt::new(TirStmtKind::Expr(expr.clone()), span)],
        span,
    }
}

/// Create an i128 literal expression by calling `i128::from_i64(value)`
pub(super) fn create_i128_literal(value: i128, type_id: TypeId, span: Span) -> TirExpr {
    // For values that fit in i64, use from_i64
    // For larger values, we'd need from_pair but pattern values should be small
    let i64_value = value as i64;
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: i64_value.cast_unsigned(),
            repr: value.to_string(),
        },
        TypeTable::I64,
        span,
    );
    let method_info = LocalMethodName::new("i128".to_string(), None, "from_i64".to_string());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: "i128::from_i64".to_string(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![CallArg::new(inner_literal, false)],
        },
        type_id,
        span,
    )
}

/// Create a u128 literal expression by calling `u128::from_u64(value)`
pub(super) fn create_u128_literal(value: u128, type_id: TypeId, span: Span) -> TirExpr {
    // For values that fit in u64, use from_u64
    // For larger values, we'd need from_pair but pattern values should be small
    let u64_value = value as u64;
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: u64_value,
            repr: value.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let method_info = LocalMethodName::new("u128".to_string(), None, "from_u64".to_string());
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::int128(),
                name: "u128::from_u64".to_string(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![CallArg::new(inner_literal, false)],
        },
        type_id,
        span,
    )
}

/// Create an i128 equality comparison using the Eq trait method call.
/// Produces: left.eq(&right)
fn create_i128_eq_call(
    left: TirExpr,
    right: TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    // Create &left (receiver, adjusted for &self)
    let left_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(left.type_id));
    let receiver = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(left),
        },
        left_ref_type,
        span,
    );

    // Create &right (argument)
    let right_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(right.type_id));
    let arg_ref = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(right),
        },
        right_ref_type,
        span,
    );

    // Create method call: receiver.eq(&right)
    let mangled_method_name = "i128^Eq::eq".to_string();
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: ModuleSource::int128(),
                name: mangled_method_name,
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "i128".to_string(),
                    Some("Eq".to_string()),
                    "eq".to_string(),
                )),
            },
            vec![],
            vec![CallArg::new(arg_ref, false)],
        ),
        TypeTable::BOOL,
        span,
    )
}

/// Create a u128 equality comparison using the Eq trait method call.
/// Produces: left.eq(&right)
fn create_u128_eq_call(
    left: TirExpr,
    right: TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    // Create &left (receiver, adjusted for &self)
    let left_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(left.type_id));
    let receiver = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(left),
        },
        left_ref_type,
        span,
    );

    // Create &right (argument)
    let right_ref_type = type_table
        .borrow_mut()
        .intern(ResolvedType::Ref(right.type_id));
    let arg_ref = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(right),
        },
        right_ref_type,
        span,
    );

    // Create method call: receiver.eq(&right)
    let mangled_method_name = "u128^Eq::eq".to_string();
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: ModuleSource::int128(),
                name: mangled_method_name,
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "u128".to_string(),
                    Some("Eq".to_string()),
                    "eq".to_string(),
                )),
            },
            vec![],
            vec![CallArg::new(arg_ref, false)],
        ),
        TypeTable::BOOL,
        span,
    )
}

/// Lower match expressions with i128/u128 literal patterns to if-else chains.
///
/// WebAssembly doesn't have native i128/u128 comparison instructions. Match expressions
/// with these types need to be converted to if-else chains that use explicit equality
/// comparisons via the wide arithmetic extension.
pub(super) fn lower_wide_int_match_patterns(module: &mut TirModule) {
    let type_table = module.type_table.clone();
    for func in &mut module.functions {
        if let Some(body) = &mut func.borrow_mut().body {
            lower_wide_int_in_block(body, &type_table);
        }
    }
}

fn lower_wide_int_in_block(block: &mut TirBlock, type_table: &Rc<RefCell<TypeTable>>) {
    for stmt in &mut block.stmts {
        lower_wide_int_in_stmt(stmt, type_table);
    }
}

fn lower_wide_int_in_stmt(stmt: &mut TirStmt, type_table: &Rc<RefCell<TypeTable>>) {
    match &mut stmt.kind {
        TirStmtKind::Expr(expr) => {
            lower_wide_int_in_expr(expr, type_table);
        }
        TirStmtKind::Return { value: Some(expr) } => {
            lower_wide_int_in_expr(expr, type_table);
        }
        TirStmtKind::Let { value, .. } => {
            lower_wide_int_in_expr(value, type_table);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            lower_wide_int_in_expr(condition, type_table);
            lower_wide_int_in_block(then_block, type_table);
            if let Some(else_b) = else_block {
                lower_wide_int_in_block(else_b, type_table);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            lower_wide_int_in_expr(scrutinee, type_table);
            lower_wide_int_in_block(then_block, type_table);
            if let Some(else_b) = else_block {
                lower_wide_int_in_block(else_b, type_table);
            }
        }
        TirStmtKind::Break { .. }
        | TirStmtKind::Continue
        | TirStmtKind::Return { value: None }
        | TirStmtKind::Loop { .. }
        | TirStmtKind::LabeledBlock { .. }
        | TirStmtKind::LetDestructure { .. } => {}
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn lower_wide_int_in_expr(expr: &mut TirExpr, type_table: &Rc<RefCell<TypeTable>>) {
    match &mut expr.kind {
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            // First, recursively process sub-expressions
            lower_wide_int_in_expr(scrutinee, type_table);
            for arm in arms.iter_mut() {
                if let Some(guard) = &mut arm.guard {
                    lower_wide_int_in_expr(guard, type_table);
                }
                lower_wide_int_in_expr(&mut arm.body, type_table);
            }

            // Check if scrutinee type is i128 or u128 (these are structs, not primitives)
            let scrutinee_type = type_table.borrow().get(scrutinee.type_id).clone();
            let is_wide_int = match &scrutinee_type {
                ResolvedType::Struct { name, .. } => name == "i128" || name == "u128",
                _ => false,
            };

            if !is_wide_int {
                return;
            }

            // Check if any arm has a literal pattern (not just wildcard/binding)
            let has_literal_patterns = arms.iter().any(|arm| {
                matches!(
                    &arm.pattern,
                    TirPattern::Literal(TirLiteralPattern::I128(_) | TirLiteralPattern::U128(_))
                )
            });

            if !has_literal_patterns {
                return;
            }

            // Transform match to if-else chain
            let result_type_id = expr.type_id;
            let span = expr.span;

            // Build if-else chain from the arms (in reverse order)
            let mut else_expr: Option<TirExpr> = None;

            for arm in arms.iter().rev() {
                match &arm.pattern {
                    TirPattern::Literal(TirLiteralPattern::I128(value)) => {
                        // Create: if scrutinee.eq(&value) { body } else { else_expr }
                        // Use MethodCall to i128^Eq::eq for value equality
                        let literal_expr = create_i128_literal(*value, scrutinee.type_id, span);
                        let condition = create_i128_eq_call(
                            (**scrutinee).clone(),
                            literal_expr,
                            type_table,
                            span,
                        );
                        // Wrap body in a block that returns the value
                        let then_block = expr_to_block(&arm.body, span);
                        let else_block = else_expr.as_ref().map(|e| expr_to_block(e, span));
                        let if_expr = TirExpr::new(
                            TirExprKind::If {
                                condition: Box::new(condition),
                                then_branch: then_block,
                                else_branch: else_block,
                            },
                            result_type_id,
                            span,
                        );
                        else_expr = Some(if_expr);
                    }
                    TirPattern::Literal(TirLiteralPattern::U128(value)) => {
                        // Create: if scrutinee.eq(&value) { body } else { else_expr }
                        // Use MethodCall to u128^Eq::eq for value equality
                        let literal_expr = create_u128_literal(*value, scrutinee.type_id, span);
                        let condition = create_u128_eq_call(
                            (**scrutinee).clone(),
                            literal_expr,
                            type_table,
                            span,
                        );
                        // Wrap body in a block that returns the value
                        let then_block = expr_to_block(&arm.body, span);
                        let else_block = else_expr.as_ref().map(|e| expr_to_block(e, span));
                        let if_expr = TirExpr::new(
                            TirExprKind::If {
                                condition: Box::new(condition),
                                then_branch: then_block,
                                else_branch: else_block,
                            },
                            result_type_id,
                            span,
                        );
                        else_expr = Some(if_expr);
                    }
                    TirPattern::Wildcard | TirPattern::Binding { .. } => {
                        // Default case - becomes the else branch
                        else_expr = Some(arm.body.clone());
                    }
                    _ => {
                        // Other patterns (tuple, variant) - shouldn't happen for i128/u128
                        // Keep the body as else for safety
                        else_expr = Some(arm.body.clone());
                    }
                }
            }

            // Replace the match expression with the if-else chain
            if let Some(result) = else_expr {
                *expr = result;
            }
        }
        // Recursively process other expression types
        TirExprKind::Binary { left, right, .. } => {
            lower_wide_int_in_expr(left, type_table);
            lower_wide_int_in_expr(right, type_table);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Assign { value: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            lower_wide_int_in_expr(inner, type_table);
        }
        TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
            for arg in args {
                lower_wide_int_in_expr(&mut arg.expr, type_table);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                lower_wide_int_in_expr(arg, type_table);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            lower_wide_int_in_expr(condition, type_table);
            lower_wide_int_in_block(then_branch, type_table);
            if let Some(e) = else_branch {
                lower_wide_int_in_block(e, type_table);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            lower_wide_int_in_block(block, type_table);
        }
        TirExprKind::Index { expr: arr, index } => {
            lower_wide_int_in_expr(arr, type_table);
            lower_wide_int_in_expr(index, type_table);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                lower_wide_int_in_expr(&mut field.value, type_table);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                lower_wide_int_in_expr(elem, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            lower_wide_int_in_expr(callee, type_table);
            for arg in args {
                lower_wide_int_in_expr(arg, type_table);
            }
        }
        TirExprKind::Closure { body, .. } => {
            lower_wide_int_in_expr(body, type_table);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            lower_wide_int_in_expr(functor, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                lower_wide_int_in_expr(p, type_table);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            lower_wide_int_in_expr(value, type_table);
        }
        // Lowered pattern matching nodes - recurse into sub-expressions
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            lower_wide_int_in_expr(expr, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            lower_wide_int_in_expr(scrutinee, type_table);
            for arm in arms {
                lower_wide_int_in_block(arm, type_table);
            }
            lower_wide_int_in_block(default, type_table);
        }
        // Terminals - no sub-expressions
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!("WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase")
        }
    }
}
