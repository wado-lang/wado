//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - String literal collection (for data section)
//! - Closure lowering (transform closures to functor structs with `__call` methods)
//! - i128/u128 match pattern lowering (convert to if-else chains)
//!
//! Note: Monomorphization has been moved to a separate phase (see `monomorphize.rs`)

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::name::{LocalMethodName, ModuleSource};
use crate::project::Project;
use crate::tir::FunctionRef;
use crate::tir::{
    ClosureFunctor, ResolvedType, TirBlock, TirCapture, TirExpr, TirExprKind, TirField,
    TirFunction, TirGlobal, TirLiteralPattern, TirModule, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Lower a TIR module
///
/// Performs:
/// 1. Global variable initialization lowering (extract non-constant initializers)
/// 2. Closure lowering (transform closures to functor structs with `__call` methods)
/// 3. String literal collection for the data section
pub fn lower(mut module: TirModule) -> TirModule {
    // Phase 0: Lower i128/u128 match patterns to if-else chains
    // WebAssembly doesn't have i128/u128 comparison instructions, so we convert
    // match expressions with i128/u128 literal patterns to if-else chains with
    // explicit equality comparisons that use the wide arithmetic extension.
    lower_wide_int_match_patterns(&mut module);

    // Phase 1: Lower global variable initializers
    // For non-constant initializers, this generates a __initialize_globals function
    // and injects calls to it at entry points.
    lower_global_initializers(&mut module);

    // Phase 2: Lower closures to functor structs
    // This generates synthetic structs and __call methods, and transforms:
    // - Closure expressions -> StructLiteral
    // - IndirectCall (known closure) -> MethodCall
    // Note: IndirectCall for unknown callees (function parameters) is kept as-is
    // for codegen to handle with call_ref.
    let mut closure_lowerer = ClosureLowerer::new(&module.module_source);
    closure_lowerer.lower_module(&mut module);

    // Phase 3: Lower primitive method calls to static calls
    // Transforms: receiver.method(args) -> Type::method(&receiver, args)
    // This allows codegen to treat primitive methods like any other static function.
    lower_primitive_method_calls(&mut module);

    // Phase 4: Collect string literals and their function mappings
    let mut collector = StringCollector::new();
    collector.collect_module(&module);
    let (strings, function_strings) = collector.into_results();
    module.string_literals = strings;
    module.function_strings = function_strings;

    module
}

/// Lower a Project (Project -> Project)
///
/// This is the main entry point for the lower phase. It lowers all TIR modules
/// in the project.
pub fn lower_project(mut project: Project) -> Project {
    project.tir_modules = lower_modules_indexed(project.tir_modules);

    // Post-processing: generate __initialize_modules in entry module
    generate_initialize_modules(&mut project.tir_modules);

    project
}

/// Lower multiple modules
///
/// Applies lowering (closure lowering, string collection) to each module.
pub fn lower_modules_indexed(
    modules: IndexMap<ModuleSource, TirModule>,
) -> IndexMap<ModuleSource, TirModule> {
    modules
        .into_iter()
        .map(|(module_source, module)| (module_source, lower(module)))
        .collect()
}

// ============================================================================
// Wide Integer Match Pattern Lowering
// ============================================================================

/// Helper to wrap an expression in a block (for if-else branches)
fn expr_to_block(expr: &TirExpr, span: Span) -> TirBlock {
    TirBlock {
        stmts: vec![TirStmt::new(TirStmtKind::Expr(expr.clone()), span)],
        span,
    }
}

/// Create an i128 literal expression by calling `i128::from_i64(value)`
fn create_i128_literal(value: i128, type_id: TypeId, span: Span) -> TirExpr {
    // For values that fit in i64, use from_i64
    // For larger values, we'd need from_pair but pattern values should be small
    let i64_value = value as i64;
    let inner_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: i64_value as u64,
            repr: value.to_string(),
        },
        TypeTable::I64,
        span,
    );
    let method_info = LocalMethodName::new("i128".to_string(), None, "from_i64".to_string());
    TirExpr::new(
        TirExprKind::StaticCall {
            func: FunctionRef::External {
                module_source: ModuleSource::core("prelude/int128"),
                name: "i128::from_i64".to_string(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            args: vec![inner_literal],
        },
        type_id,
        span,
    )
}

/// Create a u128 literal expression by calling `u128::from_u64(value)`
fn create_u128_literal(value: u128, type_id: TypeId, span: Span) -> TirExpr {
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
        TirExprKind::StaticCall {
            func: FunctionRef::External {
                module_source: ModuleSource::core("prelude/int128"),
                name: "u128::from_u64".to_string(),
                monomorph_info: None,
                method_info: Some(method_info),
            },
            args: vec![inner_literal],
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
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: FunctionRef::External {
                module_source: ModuleSource::core("prelude/int128"),
                name: mangled_method_name,
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "i128".to_string(),
                    Some("Eq".to_string()),
                    "eq".to_string(),
                )),
            },
            type_args: vec![],
            args: vec![arg_ref],
        },
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
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: FunctionRef::External {
                module_source: ModuleSource::core("prelude/int128"),
                name: mangled_method_name,
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "u128".to_string(),
                    Some("Eq".to_string()),
                    "eq".to_string(),
                )),
            },
            type_args: vec![],
            args: vec![arg_ref],
        },
        TypeTable::BOOL,
        span,
    )
}

/// Lower match expressions with i128/u128 literal patterns to if-else chains.
///
/// WebAssembly doesn't have native i128/u128 comparison instructions. Match expressions
/// with these types need to be converted to if-else chains that use explicit equality
/// comparisons via the wide arithmetic extension.
fn lower_wide_int_match_patterns(module: &mut TirModule) {
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
        TirStmtKind::IfPattern {
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
        TirStmtKind::While { condition, body } => {
            lower_wide_int_in_expr(condition, type_table);
            lower_wide_int_in_block(body, type_table);
        }
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            lower_wide_int_in_expr(scrutinee, type_table);
            lower_wide_int_in_block(body, type_table);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for i in init {
                lower_wide_int_in_stmt(i, type_table);
            }
            if let Some(c) = condition {
                lower_wide_int_in_expr(c, type_table);
            }
            if let Some(u) = update {
                lower_wide_int_in_expr(u, type_table);
            }
            lower_wide_int_in_block(body, type_table);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            update,
            body,
            ..
        } => {
            for i in init {
                lower_wide_int_in_stmt(i, type_table);
            }
            lower_wide_int_in_expr(scrutinee, type_table);
            if let Some(u) = update {
                lower_wide_int_in_expr(u, type_table);
            }
            lower_wide_int_in_block(body, type_table);
        }
        TirStmtKind::Break { .. }
        | TirStmtKind::Continue
        | TirStmtKind::Return { value: None }
        | TirStmtKind::Loop { .. }
        | TirStmtKind::ForOf { .. }
        | TirStmtKind::LabeledBlock { .. }
        | TirStmtKind::LetPattern { .. } => {}
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
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { value: inner } => {
            lower_wide_int_in_expr(inner, type_table);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::MethodCall { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
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
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
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
        // Terminals - no sub-expressions
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

// ============================================================================
// Primitive Method Call Lowering
// ============================================================================

/// Lower primitive method calls to static calls
///
/// Transforms: `receiver.method(args)` -> `Type::method(&receiver, args)`
///
/// This allows codegen to treat primitive type methods like any other static
/// function call, without requiring special-case handling for primitives.
fn lower_primitive_method_calls(module: &mut TirModule) {
    let type_table = Rc::clone(&module.type_table);

    // Process all functions
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = &mut func.body {
            lower_primitive_methods_in_block(body, &type_table);
        }
    }

    // Process impl block methods
    for impl_block in &mut module.impls {
        for method in &mut impl_block.methods {
            if let Some(body) = &mut method.body {
                lower_primitive_methods_in_block(body, &type_table);
            }
        }
    }
}

fn lower_primitive_methods_in_block(block: &mut TirBlock, type_table: &Rc<RefCell<TypeTable>>) {
    for stmt in &mut block.stmts {
        lower_primitive_methods_in_stmt(stmt, type_table);
    }
}

fn lower_primitive_methods_in_stmt(stmt: &mut TirStmt, type_table: &Rc<RefCell<TypeTable>>) {
    match &mut stmt.kind {
        TirStmtKind::Expr(expr) => lower_primitive_methods_in_expr(expr, type_table),
        TirStmtKind::Let { value, .. } => {
            lower_primitive_methods_in_expr(value, type_table);
        }
        TirStmtKind::While { condition, body }
        | TirStmtKind::WhilePattern {
            scrutinee: condition,
            body,
            ..
        } => {
            lower_primitive_methods_in_expr(condition, type_table);
            lower_primitive_methods_in_block(body, type_table);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for i in init {
                lower_primitive_methods_in_stmt(i, type_table);
            }
            if let Some(c) = condition {
                lower_primitive_methods_in_expr(c, type_table);
            }
            if let Some(u) = update {
                lower_primitive_methods_in_expr(u, type_table);
            }
            lower_primitive_methods_in_block(body, type_table);
        }
        TirStmtKind::Loop { body } => {
            lower_primitive_methods_in_block(body, type_table);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            lower_primitive_methods_in_expr(iterable, type_table);
            lower_primitive_methods_in_block(body, type_table);
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        }
        | TirStmtKind::IfPattern {
            scrutinee: condition,
            then_block,
            else_block,
            ..
        } => {
            lower_primitive_methods_in_expr(condition, type_table);
            lower_primitive_methods_in_block(then_block, type_table);
            if let Some(e) = else_block {
                lower_primitive_methods_in_block(e, type_table);
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            lower_primitive_methods_in_block(block, type_table);
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                lower_primitive_methods_in_expr(v, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::Return { value: None } => {}
        TirStmtKind::Return { value: Some(expr) } => {
            lower_primitive_methods_in_expr(expr, type_table)
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for i in init {
                lower_primitive_methods_in_stmt(i, type_table);
            }
            lower_primitive_methods_in_expr(scrutinee, type_table);
            lower_primitive_methods_in_block(body, type_table);
            if let Some(u) = update {
                lower_primitive_methods_in_expr(u, type_table);
            }
        }
        TirStmtKind::LetPattern { value, .. } => {
            lower_primitive_methods_in_expr(value, type_table);
        }
    }
}

fn lower_primitive_methods_in_expr(expr: &mut TirExpr, type_table: &Rc<RefCell<TypeTable>>) {
    // First, recursively process sub-expressions
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            lower_primitive_methods_in_expr(left, type_table);
            lower_primitive_methods_in_expr(right, type_table);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { value: inner } => {
            lower_primitive_methods_in_expr(inner, type_table);
        }
        TirExprKind::Assign { target, value } => {
            lower_primitive_methods_in_expr(target, type_table);
            lower_primitive_methods_in_expr(value, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            lower_primitive_methods_in_expr(value, type_table);
        }
        TirExprKind::Index { expr: arr, index } => {
            lower_primitive_methods_in_expr(arr, type_table);
            lower_primitive_methods_in_expr(index, type_table);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                lower_primitive_methods_in_expr(arg, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            lower_primitive_methods_in_expr(receiver, type_table);
            for arg in args {
                lower_primitive_methods_in_expr(arg, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            lower_primitive_methods_in_expr(callee, type_table);
            for arg in args {
                lower_primitive_methods_in_expr(arg, type_table);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            lower_primitive_methods_in_expr(condition, type_table);
            lower_primitive_methods_in_block(then_branch, type_table);
            if let Some(e) = else_branch {
                lower_primitive_methods_in_block(e, type_table);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            lower_primitive_methods_in_block(block, type_table);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            lower_primitive_methods_in_expr(scrutinee, type_table);
            for arm in arms {
                lower_primitive_methods_in_expr(&mut arm.body, type_table);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                lower_primitive_methods_in_expr(&mut field.value, type_table);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
            for elem in elements {
                lower_primitive_methods_in_expr(elem, type_table);
            }
        }
        TirExprKind::Closure { body, .. } => {
            lower_primitive_methods_in_expr(body, type_table);
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            lower_primitive_methods_in_expr(functor, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                lower_primitive_methods_in_expr(p, type_table);
            }
        }
        // Terminals - no sub-expressions
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }

    // Now check if this is a MethodCall on a primitive type
    if let TirExprKind::MethodCall {
        receiver,
        func,
        args,
        type_args: _,
    } = &mut expr.kind
    {
        let tt = type_table.borrow();

        // Get the base type of the receiver (strip references)
        let base_type_id = get_primitive_base_type(receiver.type_id, &tt);
        let base_type = tt.get(base_type_id);

        if let ResolvedType::Primitive(_) = base_type {
            // Transform MethodCall to StaticCall
            // receiver.method(args) -> Type::method(&receiver, args)

            let span = expr.span;
            let result_type = expr.type_id;

            // Check if receiver is already a reference
            let receiver_is_ref = matches!(
                tt.get(receiver.type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            );

            // We need to drop the borrow before modifying the type_table
            drop(tt);

            // Take ownership of the components
            let receiver_owned = std::mem::replace(
                receiver.as_mut(),
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
            );
            let func_owned = func.clone();
            let mut args_owned = std::mem::take(args);

            // Wrap receiver in a reference if it's not already a reference
            let receiver_with_ref = if receiver_is_ref {
                // Already a reference, use as-is
                receiver_owned
            } else {
                // Create a reference to the receiver: &receiver
                let ref_type_id = type_table.borrow_mut().make_ref(receiver_owned.type_id);
                TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Ref,
                        expr: Box::new(receiver_owned),
                    },
                    ref_type_id,
                    span,
                )
            };

            // Prepend receiver to args
            args_owned.insert(0, receiver_with_ref);

            // Replace the expression with StaticCall
            expr.kind = TirExprKind::StaticCall {
                func: func_owned,
                args: args_owned,
            };
            expr.type_id = result_type;
        }
    }
}

/// Get the base type by stripping Ref/MutRef wrappers
/// Get the primitive base type by following references and newtypes.
/// Returns the `type_id` of the underlying primitive type if found, or the original `type_id`.
fn get_primitive_base_type(type_id: TypeId, type_table: &TypeTable) -> TypeId {
    let mut current = type_id;
    loop {
        match type_table.get(current) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => current = *inner,
            ResolvedType::Newtype { base_type, .. } => current = *base_type,
            _ => return current,
        }
    }
}

// ============================================================================
// Global Variable Initialization Lowering
// ============================================================================

/// Check if an expression is a constant initializer (can be evaluated at Wasm instantiation time)
fn is_constant_initializer(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit => true,
        TirExprKind::Cast { expr: inner, .. } => is_constant_initializer(inner),
        TirExprKind::Unary { op, expr: inner } => {
            // Negation of literals is constant
            matches!(op, TirUnaryOp::Neg) && is_constant_initializer(inner)
        }
        _ => false,
    }
}

/// Create a default value expression for a type (used for lazy-initialized globals)
fn default_value_for_type(type_id: TypeId, type_table: &TypeTable, span: Span) -> TirExpr {
    let base_type = type_table.get_ultimate_base_type(type_id);
    match type_table.get(base_type) {
        ResolvedType::Primitive(prim) => match prim {
            crate::tir::PrimitiveType::I8
            | crate::tir::PrimitiveType::I16
            | crate::tir::PrimitiveType::I32
            | crate::tir::PrimitiveType::U8
            | crate::tir::PrimitiveType::U16
            | crate::tir::PrimitiveType::U32 => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::I64 | crate::tir::PrimitiveType::U64 => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::I128 | crate::tir::PrimitiveType::U128 => {
                // i128/u128 need special handling - call from_i64(0) / from_u64(0)
                if matches!(prim, crate::tir::PrimitiveType::I128) {
                    create_i128_literal(0, type_id, span)
                } else {
                    create_u128_literal(0, type_id, span)
                }
            }
            crate::tir::PrimitiveType::F32 => TirExpr::new(
                TirExprKind::FloatLiteral {
                    value: 0.0,
                    repr: "0.0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::F64 => TirExpr::new(
                TirExprKind::FloatLiteral {
                    value: 0.0,
                    repr: "0.0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::Bool => {
                TirExpr::new(TirExprKind::BoolLiteral(false), type_id, span)
            }
            crate::tir::PrimitiveType::Char => {
                TirExpr::new(TirExprKind::CharLiteral('\0'), type_id, span)
            }
        },
        ResolvedType::Unit => TirExpr::new(TirExprKind::Unit, type_id, span),
        // For reference types (String, Array, struct, etc.), use null
        _ => TirExpr::new(TirExprKind::Null, type_id, span),
    }
}

/// Check if a type is a reference type (needs nullable Wasm type for lazy init)
fn is_reference_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    let base_type = type_table.get_ultimate_base_type(type_id);
    match type_table.get(base_type) {
        ResolvedType::Primitive(_) | ResolvedType::Unit | ResolvedType::Never => false,
        // Struct, Array, String, etc. are reference types in Wasm GC
        _ => true,
    }
}

/// Lower global variable initializers
///
/// For non-constant initializers, this:
/// 1. Replaces the initializer with a default value
/// 2. Generates a `__initialize_module` function containing the actual initialization
///
/// Note: The `__initialize_modules` function that calls all modules' `__initialize_module`
/// is generated in the post-processing step (see `generate_initialize_modules`).
fn lower_global_initializers(module: &mut TirModule) {
    let type_table = module.type_table.borrow();

    // Collect non-constant initializers with their indices for topological sorting
    let mut lazy_inits: Vec<(usize, String, ModuleSource, TypeId, TirExpr)> = Vec::new();

    for (idx, global) in module.globals.iter_mut().enumerate() {
        if !is_constant_initializer(&global.initializer) {
            // Save the original initializer with index
            lazy_inits.push((
                idx,
                global.name.clone(),
                global.module_source.clone(),
                global.ty,
                global.initializer.clone(),
            ));
            // Replace with default value
            global.initializer = default_value_for_type(global.ty, &type_table, global.span);
            // Lazy-init globals must be Wasm-mutable (even if Wado-immutable)
            global.mutable = true;
            // Reference types need nullable Wasm type for lazy init
            if is_reference_type(global.ty, &type_table) {
                global.is_nullable = true;
            }
        }
    }

    drop(type_table);

    // If no lazy initializers, nothing to do
    if lazy_inits.is_empty() {
        return;
    }

    // Topologically sort the lazy initializers based on dependencies
    let sorted_inits = topological_sort_global_inits(&lazy_inits, &module.globals);

    // Generate __initialize_module function
    let span = Span::new(0, 0, 1, 1);
    let mut init_stmts: Vec<TirStmt> = Vec::new();

    for (_, name, module_source, _, initializer) in sorted_inits {
        // Create: global_name = initializer;
        let global_set = TirExpr::new(
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value: Box::new(initializer),
            },
            TypeTable::UNIT,
            span,
        );
        init_stmts.push(TirStmt::new(TirStmtKind::Expr(global_set), span));
    }

    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    let init_func = TirFunction {
        name: "__initialize_module".to_string(),
        is_pub: true, // pub so it can be called from entry module's __initialize_modules
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: Vec::new(),
        return_type: TypeTable::UNIT,
        effects: Vec::new(),
        body: Some(init_body),
        span,
        local_count: 0,
        local_types: Vec::new(),
        address_taken_locals: std::collections::HashSet::new(),
        needed_copy_types: std::collections::HashSet::new(),
    };

    module.functions.push(Rc::new(RefCell::new(init_func)));
}

/// Collect global variable references from an expression
fn collect_global_refs(expr: &TirExpr, refs: &mut HashSet<String>) {
    match &expr.kind {
        TirExprKind::GlobalVarGet { name, .. } => {
            refs.insert(name.clone());
        }
        // Recursively search in sub-expressions
        TirExprKind::Binary { left, right, .. } => {
            collect_global_refs(left, refs);
            collect_global_refs(right, refs);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_global_refs(inner, refs);
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                collect_global_refs(arg, refs);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_global_refs(receiver, refs);
            for arg in args {
                collect_global_refs(arg, refs);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_global_refs(&field.value, refs);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                collect_global_refs(elem, refs);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_global_refs(elem, refs);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_global_refs(condition, refs);
            for stmt in &then_branch.stmts {
                if let TirStmtKind::Expr(e) = &stmt.kind {
                    collect_global_refs(e, refs);
                }
            }
            if let Some(else_blk) = else_branch {
                for stmt in &else_blk.stmts {
                    if let TirStmtKind::Expr(e) = &stmt.kind {
                        collect_global_refs(e, refs);
                    }
                }
            }
        }
        TirExprKind::Block(block) => {
            for stmt in &block.stmts {
                if let TirStmtKind::Expr(e) = &stmt.kind {
                    collect_global_refs(e, refs);
                }
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_global_refs(inner, refs);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_global_refs(inner, refs);
            collect_global_refs(index, refs);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_global_refs(inner, refs);
        }
        TirExprKind::Assign { target, value } => {
            collect_global_refs(target, refs);
            collect_global_refs(value, refs);
        }
        // Leaf expressions - no sub-expressions
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. } => {}
        // Other expressions - skip for now
        _ => {}
    }
}

/// Topologically sort global initializers based on dependencies.
///
/// Returns the initializers in an order where dependencies are initialized first.
fn topological_sort_global_inits(
    lazy_inits: &[(usize, String, ModuleSource, TypeId, TirExpr)],
    _all_globals: &[TirGlobal],
) -> Vec<(usize, String, ModuleSource, TypeId, TirExpr)> {
    if lazy_inits.len() <= 1 {
        return lazy_inits.to_vec();
    }

    // Build a map from global name to its index in lazy_inits
    let name_to_idx: HashMap<String, usize> = lazy_inits
        .iter()
        .enumerate()
        .map(|(i, (_, name, _, _, _))| (name.clone(), i))
        .collect();

    // Build dependency graph: deps[i] = set of indices that i depends on
    let mut deps: Vec<HashSet<usize>> = vec![HashSet::new(); lazy_inits.len()];

    for (i, (_, _, _, _, initializer)) in lazy_inits.iter().enumerate() {
        let mut refs = HashSet::new();
        collect_global_refs(initializer, &mut refs);

        for ref_name in refs {
            // Only consider dependencies on other lazy-init globals in this module
            if let Some(&dep_idx) = name_to_idx.get(&ref_name)
                && dep_idx != i
            {
                deps[i].insert(dep_idx);
            }
        }
    }

    // Kahn's algorithm for topological sort
    let mut in_degree: Vec<usize> = deps.iter().map(HashSet::len).collect();
    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut sorted = Vec::with_capacity(lazy_inits.len());

    while let Some(idx) = queue.pop_front() {
        sorted.push(lazy_inits[idx].clone());

        // Update dependents
        for (i, dep_set) in deps.iter().enumerate() {
            if dep_set.contains(&idx) {
                in_degree[i] -= 1;
                if in_degree[i] == 0 {
                    queue.push_back(i);
                }
            }
        }
    }

    // Check for cycles
    if sorted.len() < lazy_inits.len() {
        // Cycle detected - report which globals are involved
        let in_cycle: Vec<&str> = lazy_inits
            .iter()
            .enumerate()
            .filter(|(i, _)| in_degree[*i] > 0)
            .map(|(_, (_, name, _, _, _))| name.as_str())
            .collect();
        panic!(
            "Circular dependency detected among global variables: {}",
            in_cycle.join(", ")
        );
    }

    sorted
}

/// Generate `__initialize_modules` function in the entry module.
///
/// This function:
/// 1. Checks an initialization flag and returns early if already initialized
/// 2. Calls each module's `__initialize_module` in topological order
/// 3. Sets the initialization flag to true
/// 4. Injects a call to `__initialize_modules` at the start of entry point functions
fn generate_initialize_modules(modules: &mut IndexMap<ModuleSource, TirModule>) {
    // Find the entry module
    let entry_source = modules.keys().find(|ms| ms.is_entry_point()).cloned();

    let Some(entry_source) = entry_source else {
        return; // No entry module
    };

    // Collect all modules that have __initialize_module function
    let mut modules_with_init: Vec<ModuleSource> = Vec::new();
    for (module_source, module) in modules.iter() {
        let has_init = module
            .functions
            .iter()
            .any(|f| f.borrow().name == "__initialize_module");
        if has_init {
            modules_with_init.push(module_source.clone());
        }
    }

    // If no modules have initialization, nothing to do
    if modules_with_init.is_empty() {
        return;
    }

    // Sort modules so that dependencies are initialized before dependents.
    // For now, put the entry module last (it typically imports from other modules).
    // Non-entry modules are sorted by their appearance order in the IndexMap
    // (which the loader already sorts by dependency).
    modules_with_init.sort_by_key(|ms| {
        if ms == &entry_source {
            1 // Entry module last
        } else {
            0 // Other modules first
        }
    });

    let span = Span::new(0, 0, 1, 1);

    // Get mutable reference to entry module
    let entry_module = modules.get_mut(&entry_source).unwrap();

    // Create __modules_initialized flag global
    let init_flag_global = TirGlobal {
        name: "__modules_initialized".to_string(),
        ty: TypeTable::BOOL,
        initializer: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, span),
        mutable: true,
        is_pub: false,
        module_source: entry_source.clone(),
        span,
        is_nullable: false,
    };
    entry_module.globals.push(init_flag_global);

    // Build __initialize_modules function body
    let mut init_stmts: Vec<TirStmt> = Vec::new();

    // Check flag: if __modules_initialized { return; }
    let flag_check = TirExpr::new(
        TirExprKind::GlobalVarGet {
            module_source: entry_source.clone(),
            name: "__modules_initialized".to_string(),
        },
        TypeTable::BOOL,
        span,
    );
    let early_return_stmt = TirStmt::new(TirStmtKind::Return { value: None }, span);
    let early_return_block = TirBlock {
        stmts: vec![early_return_stmt],
        span,
    };
    let if_already_init = TirStmt::new(
        TirStmtKind::If {
            condition: flag_check,
            then_block: early_return_block,
            else_block: None,
        },
        span,
    );
    init_stmts.push(if_already_init);

    // Call each module's __initialize_module
    for module_source in &modules_with_init {
        let call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef::External {
                    module_source: module_source.clone(),
                    name: "__initialize_module".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: Vec::new(),
                args: Vec::new(),
            },
            TypeTable::UNIT,
            span,
        );
        init_stmts.push(TirStmt::new(TirStmtKind::Expr(call), span));
    }

    // Set flag: __modules_initialized = true;
    let set_flag = TirExpr::new(
        TirExprKind::GlobalVarSet {
            module_source: entry_source.clone(),
            name: "__modules_initialized".to_string(),
            value: Box::new(TirExpr::new(
                TirExprKind::BoolLiteral(true),
                TypeTable::BOOL,
                span,
            )),
        },
        TypeTable::UNIT,
        span,
    );
    init_stmts.push(TirStmt::new(TirStmtKind::Expr(set_flag), span));

    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    let init_modules_func = TirFunction {
        name: "__initialize_modules".to_string(),
        is_pub: false, // Not pub - internal to entry module
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: Vec::new(),
        return_type: TypeTable::UNIT,
        effects: Vec::new(),
        body: Some(init_body),
        span,
        local_count: 0,
        local_types: Vec::new(),
        address_taken_locals: std::collections::HashSet::new(),
        needed_copy_types: std::collections::HashSet::new(),
    };

    entry_module
        .functions
        .push(Rc::new(RefCell::new(init_modules_func)));

    // Inject call to __initialize_modules at the start of entry point functions
    let init_call = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef::External {
                module_source: entry_source.clone(),
                name: "__initialize_modules".to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: Vec::new(),
            args: Vec::new(),
        },
        TypeTable::UNIT,
        span,
    );
    let init_call_stmt = TirStmt::new(TirStmtKind::Expr(init_call), span);

    for func_rc in &entry_module.functions {
        let mut func = func_rc.borrow_mut();
        let is_entry = func.name == "run" || func.name.starts_with("__test_");

        if is_entry && let Some(ref mut body) = func.body {
            body.stmts.insert(0, init_call_stmt.clone());
        }
    }
}

// ============================================================================
// Closure Lowering
// ============================================================================

/// Information about a closure collected during the first pass
#[derive(Debug, Clone)]
struct CollectedClosure {
    /// Unique closure ID (assigned in order of collection)
    id: u32,
    /// Parameters of the closure
    params: Vec<(String, TypeId)>,
    /// The closure body expression (cloned for __call method generation)
    body: TirExpr,
    /// Captures from the closure
    captures: Vec<TirCapture>,
    /// Return type of the closure
    return_type: TypeId,
    /// Original function type (for compatibility)
    func_type_id: TypeId,
    /// Span of the original closure
    span: Span,
}

// FunctorInfo moved to tir::ClosureFunctor

/// Key for fn-param specialization: (callee function name, parameter index -> functor type ID)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FnParamSpecKey {
    /// Name of the callee function
    callee_name: String,
    /// Map from parameter index to functor struct type ID (for fn-type params with closure args)
    functor_types: Vec<(u32, TypeId)>,
}

/// Lowers closures to functor structs with `__call` methods.
///
/// For each closure, this generates:
/// 1. A synthetic struct `__Closure_N` with fields for captured variables
/// 2. A `__call` method containing the transformed closure body
///
/// Transformations (selective - only for closures stored in locals and called directly):
/// - `Closure { params, body, captures }` → `StructLiteral { __Closure_N, capture_values }`
/// - `Capture { index }` (in body) → `FieldAccess { self, __capture_{index} }`
/// - `IndirectCall { callee, args }` (known closure) → `MethodCall { callee, __call, args }`
///
/// Closures passed as function arguments are transformed via fn-param specialization:
/// - A specialized version of the callee is generated with functor struct params
/// - The call is updated to use the specialized function with `StructLiteral` args
struct ClosureLowerer {
    /// Counter for generating unique closure IDs
    closure_counter: u32,
    /// Module source for generated items
    module_source: ModuleSource,
    /// Collected closures during first pass
    collected_closures: Vec<CollectedClosure>,
    /// Generated functor information (populated after struct/method generation)
    /// These will be stored in `module.closure_functors` for the optimizer
    functor_infos: Vec<ClosureFunctor>,
    /// Map from local variable index to closure ID (for tracking closures stored in locals)
    local_to_closure: HashMap<u32, u32>,
    /// Closure IDs that can be specialized (stored in locals, called directly).
    /// Non-specializable closures use `ClosureToCanonical` for type-erased representation.
    specializable: std::collections::HashSet<u32>,
    /// Generated structs to add to module
    generated_structs: Vec<TirStruct>,
    /// Generated functions to add to module
    generated_functions: Vec<Rc<RefCell<TirFunction>>>,
    /// Map from fn-param spec key to specialized function name
    fn_param_specializations: HashMap<FnParamSpecKey, String>,
}

impl ClosureLowerer {
    fn new(module_source: &ModuleSource) -> Self {
        Self {
            closure_counter: 0,
            module_source: module_source.clone(),
            collected_closures: Vec::new(),
            functor_infos: Vec::new(),
            local_to_closure: HashMap::new(),
            specializable: std::collections::HashSet::new(),
            generated_structs: Vec::new(),
            generated_functions: Vec::new(),
            fn_param_specializations: HashMap::new(),
        }
    }

    /// Lower all closures in a module
    fn lower_module(&mut self, module: &mut TirModule) {
        // First pass: collect all closures
        // Reset counter for consistent ordering
        self.closure_counter = 0;
        self.collected_closures.clear();

        let func_refs: Vec<_> = module.functions.clone();
        for func_rc in &func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.collect_closures_in_block(body);
            }
        }

        // Also collect from impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.collect_closures_in_block(body);
                }
            }
        }

        // Generate functor structs and __call methods
        self.generate_functor_items(&mut module.type_table.borrow_mut());

        // Second pass: analyze which closures are safe to transform
        // Safe: stored in a local and called directly
        // Unsafe: passed as argument to a function (requires monomorphization, see WEP)
        self.closure_counter = 0;
        for func_rc in &func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.local_to_closure.clear();
                self.analyze_closure_safety_block(body);
            }
        }

        // Also analyze impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.local_to_closure.clear();
                    self.analyze_closure_safety_block(body);
                }
            }
        }

        // Phase 2.5: Generate specialized functions for fn-param monomorphization
        // For closures passed as fn-type arguments, generate specialized callees
        self.generate_fn_param_specializations(
            &func_refs,
            &module.impls,
            &mut module.type_table.borrow_mut(),
        );

        // Third pass: transform closures to struct literals and IndirectCall to MethodCall
        self.closure_counter = 0;

        for func_rc in &func_refs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                self.local_to_closure.clear();
                self.transform_block(body, &mut module.type_table.borrow_mut());
            }
            // Update local_types after transformation
            self.update_local_types(&mut func);
        }

        // Also transform impl methods
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    self.local_to_closure.clear();
                    self.transform_block(body, &mut module.type_table.borrow_mut());
                }
                // Update local_types after transformation
                self.update_local_types(method);
            }
        }

        // Fourth pass: transform remaining Closure nodes to ClosureToCanonical
        // These are closures that weren't specialized (fn-param stored in struct field)
        for func_rc in &func_refs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                self.transform_remaining_closures_block(body);
            }
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    self.transform_remaining_closures_block(body);
                }
            }
        }

        // Store functor metadata in module for the optimizer to use.
        // This enables closure inlining by providing the __call method body.
        module.closure_functors = std::mem::take(&mut self.functor_infos);

        // Add ALL generated structs and functions to the module
        module
            .structs
            .extend(std::mem::take(&mut self.generated_structs));
        module
            .functions
            .extend(std::mem::take(&mut self.generated_functions));
    }

    /// Update `local_types` in a function after closure transformation
    fn update_local_types(&self, func: &mut TirFunction) {
        // For each local that stored a closure and was transformed to a struct,
        // update its type from function type to struct type
        for (local_idx, closure_id) in &self.local_to_closure {
            if self.specializable.contains(closure_id)
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
                && let Some(type_id) = func.local_types.get_mut(*local_idx as usize)
            {
                // Functors are reference types
                *type_id = functor.ref_type_id;
            }
        }
    }

    // ========================================================================
    // First Pass: Collect Closures
    // ========================================================================

    fn collect_closures_in_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.collect_closures_in_stmt(stmt);
        }
    }

    fn collect_closures_in_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_closures_in_expr(value);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.collect_closures_in_expr(expr);
            }
            TirStmtKind::Return { value: None }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_closures_in_expr(condition);
                self.collect_closures_in_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_closures_in_block(else_blk);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.collect_closures_in_expr(condition);
                self.collect_closures_in_block(body);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.collect_closures_in_stmt(s);
                }
                if let Some(cond) = condition {
                    self.collect_closures_in_expr(cond);
                }
                self.collect_closures_in_block(body);
                if let Some(upd) = update {
                    self.collect_closures_in_expr(upd);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.collect_closures_in_block(body);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.collect_closures_in_expr(iterable);
                self.collect_closures_in_block(body);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_closures_in_expr(scrutinee);
                self.collect_closures_in_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_closures_in_block(else_blk);
                }
            }
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                self.collect_closures_in_expr(scrutinee);
                self.collect_closures_in_block(body);
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                for s in init {
                    self.collect_closures_in_stmt(s);
                }
                self.collect_closures_in_expr(scrutinee);
                self.collect_closures_in_block(body);
                if let Some(upd) = update {
                    self.collect_closures_in_expr(upd);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.collect_closures_in_expr(value);
            }
        }
    }

    fn collect_closures_in_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Closure {
                params,
                body,
                captures,
                functor_id: _,
            } => {
                // Assign ID and collect this closure
                let closure_id = self.closure_counter;
                self.closure_counter += 1;

                self.collected_closures.push(CollectedClosure {
                    id: closure_id,
                    params: params.clone(),
                    body: (**body).clone(),
                    captures: captures.clone(),
                    return_type: body.type_id,
                    func_type_id: expr.type_id,
                    span: expr.span,
                });

                // Recursively collect nested closures in the body
                self.collect_closures_in_expr(body);
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_closures_in_expr(left);
                self.collect_closures_in_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                self.collect_closures_in_expr(inner);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.collect_closures_in_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.collect_closures_in_expr(receiver);
                for arg in args {
                    self.collect_closures_in_expr(arg);
                }
            }
            TirExprKind::Block(block) => {
                self.collect_closures_in_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_closures_in_expr(condition);
                self.collect_closures_in_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.collect_closures_in_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_closures_in_expr(&field.value);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_closures_in_expr(elem);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_closures_in_expr(target);
                self.collect_closures_in_expr(value);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_closures_in_expr(array);
                self.collect_closures_in_expr(index);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_closures_in_expr(scrutinee);
                for arm in arms {
                    self.collect_closures_in_expr(&arm.body);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_closures_in_expr(callee);
                for arg in args {
                    self.collect_closures_in_expr(arg);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_closures_in_expr(functor);
            }
            TirExprKind::OptionSome { value } | TirExprKind::Move { value } => {
                self.collect_closures_in_expr(value);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_closures_in_expr(payload_expr);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_closures_in_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_closures_in_expr(value);
            }
            // Terminals - no closures
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
        }
    }

    // ========================================================================
    // Second Pass: Analyze Closure Safety
    // ========================================================================

    /// Analyze a block to identify which closures are safe to transform
    fn analyze_closure_safety_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.analyze_closure_safety_stmt(stmt);
        }
    }

    fn analyze_closure_safety_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                // If this local stores a closure, track it
                if matches!(value.kind, TirExprKind::Closure { .. }) {
                    let closure_id = self.closure_counter;
                    self.local_to_closure.insert(*local_index, closure_id);
                    // Initially mark as safe; will be removed if passed as argument
                    self.specializable.insert(closure_id);
                }
                // Analyze the value expression
                self.analyze_closure_safety_expr(value, false);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.analyze_closure_safety_expr(expr, false);
            }
            TirStmtKind::Return { value: None }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.analyze_closure_safety_expr(condition, false);
                self.analyze_closure_safety_block(then_block);
                if let Some(else_blk) = else_block {
                    self.analyze_closure_safety_block(else_blk);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.analyze_closure_safety_expr(condition, false);
                self.analyze_closure_safety_block(body);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.analyze_closure_safety_stmt(s);
                }
                if let Some(cond) = condition {
                    self.analyze_closure_safety_expr(cond, false);
                }
                self.analyze_closure_safety_block(body);
                if let Some(upd) = update {
                    self.analyze_closure_safety_expr(upd, false);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.analyze_closure_safety_block(body);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.analyze_closure_safety_expr(iterable, false);
                self.analyze_closure_safety_block(body);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.analyze_closure_safety_expr(scrutinee, false);
                self.analyze_closure_safety_block(then_block);
                if let Some(else_blk) = else_block {
                    self.analyze_closure_safety_block(else_blk);
                }
            }
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                self.analyze_closure_safety_expr(scrutinee, false);
                self.analyze_closure_safety_block(body);
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                for s in init {
                    self.analyze_closure_safety_stmt(s);
                }
                self.analyze_closure_safety_expr(scrutinee, false);
                self.analyze_closure_safety_block(body);
                if let Some(upd) = update {
                    self.analyze_closure_safety_expr(upd, false);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.analyze_closure_safety_expr(value, false);
            }
        }
    }

    /// Analyze an expression for closure safety
    /// `in_arg_position` is true when this expression is being passed as an argument
    fn analyze_closure_safety_expr(&mut self, expr: &TirExpr, in_arg_position: bool) {
        match &expr.kind {
            TirExprKind::Closure { body, .. } => {
                // Count this closure
                let closure_id = self.closure_counter;
                self.closure_counter += 1;

                // If a closure appears directly as an argument, it's not safe to transform
                if in_arg_position {
                    self.specializable.remove(&closure_id);
                }

                // Recursively analyze the body
                self.analyze_closure_safety_expr(body, false);
            }
            TirExprKind::Local { index, .. } => {
                // If a local that holds a closure is passed as an argument, mark it unsafe
                if in_arg_position && let Some(closure_id) = self.local_to_closure.get(index) {
                    self.specializable.remove(closure_id);
                }
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                // Arguments are in argument position
                for arg in args {
                    self.analyze_closure_safety_expr(arg, true);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                // Receiver is not in argument position for method calls
                self.analyze_closure_safety_expr(receiver, false);
                // Arguments are in argument position
                for arg in args {
                    self.analyze_closure_safety_expr(arg, true);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                // Callee is not an argument (it's what's being called)
                self.analyze_closure_safety_expr(callee, false);
                // Arguments are in argument position
                for arg in args {
                    self.analyze_closure_safety_expr(arg, true);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.analyze_closure_safety_expr(functor, in_arg_position);
            }
            TirExprKind::Binary { left, right, .. } => {
                self.analyze_closure_safety_expr(left, false);
                self.analyze_closure_safety_expr(right, false);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                self.analyze_closure_safety_expr(inner, false);
            }
            TirExprKind::Block(block) => {
                self.analyze_closure_safety_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.analyze_closure_safety_expr(condition, false);
                self.analyze_closure_safety_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.analyze_closure_safety_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.analyze_closure_safety_expr(&field.value, false);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.analyze_closure_safety_expr(elem, false);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.analyze_closure_safety_expr(target, false);
                self.analyze_closure_safety_expr(value, false);
            }
            TirExprKind::Index { expr: array, index } => {
                self.analyze_closure_safety_expr(array, false);
                self.analyze_closure_safety_expr(index, false);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.analyze_closure_safety_expr(scrutinee, false);
                for arm in arms {
                    self.analyze_closure_safety_expr(&arm.body, false);
                }
            }
            TirExprKind::OptionSome { value } | TirExprKind::Move { value } => {
                self.analyze_closure_safety_expr(value, false);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.analyze_closure_safety_expr(payload_expr, false);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.analyze_closure_safety_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.analyze_closure_safety_expr(value, false);
            }
            // Terminals
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
        }
    }

    // ========================================================================
    // Generate Functor Structs and __call Methods
    // ========================================================================

    fn generate_functor_items(&mut self, type_table: &mut TypeTable) {
        for collected in &self.collected_closures.clone() {
            // Extract the actual return type from the closure's function type
            // This is more reliable than body.type_id for closures with block bodies
            let return_type = match type_table.get(collected.func_type_id) {
                crate::tir::ResolvedType::Function { return_type, .. } => *return_type,
                _ => collected.return_type, // Fallback to body type
            };

            // Generate struct name and type
            let struct_name = format!("__Closure_{}", collected.id);
            let struct_type_id =
                type_table.make_struct(struct_name.clone(), self.module_source.clone());

            // Generate struct definition with capture fields
            let fields: Vec<TirField> = collected
                .captures
                .iter()
                .enumerate()
                .map(|(i, cap)| TirField {
                    name: format!("__capture_{i}"),
                    type_id: cap.type_id,
                    index: i as u32,
                    span: collected.span,
                })
                .collect();

            let tir_struct = TirStruct {
                name: struct_name.clone(),
                is_pub: false,
                type_params: Vec::new(),
                monomorph_info: None,
                fields,
                span: collected.span,
            };
            self.generated_structs.push(tir_struct);

            // Generate __call method
            // Use a qualified name for the function to avoid collisions in the inliner's candidate map
            let simple_method_name = "__call".to_string();
            let qualified_method_name = format!("{struct_name}::__call");
            let self_ref_type = type_table.make_ref(struct_type_id);

            // Parameters: self + closure params
            let mut params = Vec::new();
            params.push(TirParam {
                name: "self".to_string(),
                type_id: self_ref_type,
                local_index: 0,
                span: collected.span,
            });

            for (i, (name, type_id)) in collected.params.iter().enumerate() {
                params.push(TirParam {
                    name: name.clone(),
                    type_id: *type_id,
                    local_index: (i + 1) as u32,
                    span: collected.span,
                });
            }

            // Transform the body: Capture { index } -> FieldAccess { self, __capture_{index} }
            let transformed_body = self.transform_closure_body(
                &collected.body,
                &collected.captures,
                struct_type_id,
                self_ref_type,
                &collected.span,
            );

            // Handle body wrapping based on body type
            // For block bodies, extract statements directly to preserve Return handling during inlining
            // For expression bodies, wrap in Return
            let body_stmts = match &transformed_body.kind {
                TirExprKind::Block(block) => {
                    // Block body: use statements directly (they already contain Return statements)
                    // This is important for inlining: the inliner's remap_stmt_with_label
                    // converts Return to Break, but only at the statement level, not inside
                    // Block expressions.
                    block.stmts.clone()
                }
                _ => {
                    if return_type == TypeTable::UNIT {
                        // Unit return: just evaluate the expression for side effects
                        vec![TirStmt::new(
                            TirStmtKind::Expr(transformed_body),
                            collected.span,
                        )]
                    } else {
                        // Expression body that returns a value
                        vec![TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(transformed_body),
                            },
                            collected.span,
                        )]
                    }
                }
            };

            let body_block = TirBlock::new(body_stmts, collected.span);

            let local_count = 1 + collected.params.len() as u32;
            let mut local_types = vec![self_ref_type];
            local_types.extend(collected.params.iter().map(|(_, t)| *t));

            // method_info tells codegen how to register this function with the proper mangled name
            let method_info = LocalMethodName::new(
                struct_name.clone(),        // __Closure_0
                None,                       // no trait
                simple_method_name.clone(), // __call (just the method name)
            );

            let call_method = TirFunction {
                name: qualified_method_name,
                is_pub: false,
                type_params: Vec::new(),
                impl_type_params: Vec::new(),
                monomorph_info: None,
                method_info: Some(method_info),
                params,
                return_type,
                effects: Vec::new(),
                body: Some(body_block),
                span: collected.span,
                local_count,
                local_types,
                address_taken_locals: std::collections::HashSet::new(),
                needed_copy_types: std::collections::HashSet::new(),
            };

            let call_method_rc = Rc::new(RefCell::new(call_method));
            self.generated_functions.push(Rc::clone(&call_method_rc));

            self.functor_infos.push(ClosureFunctor {
                id: collected.id,
                struct_name,
                struct_type_id,
                ref_type_id: self_ref_type,
                call_method: call_method_rc,
                captures: collected.captures.clone(),
            });
        }
    }

    /// Transform closure body: replace Capture with `FieldAccess` on self
    fn transform_closure_body(
        &self,
        expr: &TirExpr,
        captures: &[TirCapture],
        struct_type_id: TypeId,
        self_ref_type: TypeId,
        span: &Span,
    ) -> TirExpr {
        match &expr.kind {
            TirExprKind::Capture { index, name: _ } => {
                // Transform: Capture { index } -> self.__capture_{index}
                let self_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: 0, // self is at local 0
                        name: "self".to_string(),
                    },
                    self_ref_type,
                    *span,
                );

                let field_name = format!("__capture_{index}");
                let cap_type = captures
                    .get(*index as usize)
                    .map_or(TypeTable::UNKNOWN, |c| c.type_id);

                TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(self_expr),
                        field_index: *index,
                        field_name,
                    },
                    cap_type,
                    *span,
                )
            }
            TirExprKind::Binary { left, op, right } => TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(self.transform_closure_body(
                        left,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    op: *op,
                    right: Box::new(self.transform_closure_body(
                        right,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Unary { op, expr: inner } => TirExpr::new(
                TirExprKind::Unary {
                    op: *op,
                    expr: Box::new(self.transform_closure_body(
                        inner,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Local { index, name } => {
                // Adjust local indices: closure params start at local 1 (after self)
                // Original closure param 0 becomes local 1, etc.
                TirExpr::new(
                    TirExprKind::Local {
                        index: index + 1,
                        name: name.clone(),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Block(block) => {
                let transformed_block = self.transform_closure_body_block(
                    block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                );
                TirExpr::new(
                    TirExprKind::Block(transformed_block),
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(self.transform_closure_body(
                        condition,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    then_branch: self.transform_closure_body_block(
                        then_branch,
                        captures,
                        struct_type_id,
                        self_ref_type,
                    ),
                    else_branch: else_branch.as_ref().map(|b| {
                        self.transform_closure_body_block(
                            b,
                            captures,
                            struct_type_id,
                            self_ref_type,
                        )
                    }),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(self.transform_closure_body(
                        inner,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    target_type: *target_type,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                let new_fields = fields
                    .iter()
                    .map(|f| crate::tir::TirStructField {
                        name: f.name.clone(),
                        value: self.transform_closure_body(
                            &f.value,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        ),
                        field_index: f.field_index,
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::StructLiteral {
                        struct_type: *struct_type,
                        struct_name: struct_name.clone(),
                        fields: new_fields,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Call {
                func,
                type_args,
                args,
            } => {
                let new_args = args
                    .iter()
                    .map(|a| {
                        self.transform_closure_body(
                            a,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::Call {
                        func: func.clone(),
                        type_args: type_args.clone(),
                        args: new_args,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
            } => {
                let new_receiver = self.transform_closure_body(
                    receiver,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_args = args
                    .iter()
                    .map(|a| {
                        self.transform_closure_body(
                            a,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::MethodCall {
                        receiver: Box::new(new_receiver),
                        func: func.clone(),
                        type_args: type_args.clone(),
                        args: new_args,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                field_name,
            } => {
                let new_inner = self.transform_closure_body(
                    inner,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(new_inner),
                        field_index: *field_index,
                        field_name: field_name.clone(),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Assign { target, value } => {
                let new_target = self.transform_closure_body(
                    target,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_value = self.transform_closure_body(
                    value,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::Assign {
                        target: Box::new(new_target),
                        value: Box::new(new_value),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Index { expr: array, index } => {
                let new_array = self.transform_closure_body(
                    array,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_index = self.transform_closure_body(
                    index,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::Index {
                        expr: Box::new(new_array),
                        index: Box::new(new_index),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::ArrayLiteral { elements } => {
                let new_elements = elements
                    .iter()
                    .map(|e| {
                        self.transform_closure_body(
                            e,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::ArrayLiteral {
                        elements: new_elements,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::TupleLiteral { elements } => {
                let new_elements = elements
                    .iter()
                    .map(|e| {
                        self.transform_closure_body(
                            e,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::TupleLiteral {
                        elements: new_elements,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            // Terminals that don't need transformation
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Global { .. }
            | TirExprKind::EnumConstruct { .. } => expr.clone(),
            // For remaining expression types, clone as-is
            // (IndirectCall, Closure, etc. - rare in closure bodies)
            _ => expr.clone(),
        }
    }

    /// Transform statements within a closure body block
    fn transform_closure_body_block(
        &self,
        block: &TirBlock,
        captures: &[TirCapture],
        struct_type_id: TypeId,
        self_ref_type: TypeId,
    ) -> TirBlock {
        let stmts: Vec<TirStmt> = block
            .stmts
            .iter()
            .map(|stmt| {
                self.transform_closure_body_stmt(stmt, captures, struct_type_id, self_ref_type)
            })
            .collect();
        TirBlock::new(stmts, block.span)
    }

    /// Transform a statement within a closure body
    fn transform_closure_body_stmt(
        &self,
        stmt: &TirStmt,
        captures: &[TirCapture],
        struct_type_id: TypeId,
        self_ref_type: TypeId,
    ) -> TirStmt {
        let span = &stmt.span;
        let kind = match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                name,
                is_mut,
                is_reactive,
                type_id,
                value,
            } => TirStmtKind::Let {
                local_index: local_index + 1, // Shift by 1 for self parameter
                name: name.clone(),
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.transform_closure_body(
                    value,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
            },
            TirStmtKind::Expr(expr) => TirStmtKind::Expr(self.transform_closure_body(
                expr,
                captures,
                struct_type_id,
                self_ref_type,
                span,
            )),
            TirStmtKind::Return { value } => TirStmtKind::Return {
                value: value.as_ref().map(|v| {
                    self.transform_closure_body(v, captures, struct_type_id, self_ref_type, span)
                }),
            },
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => TirStmtKind::If {
                condition: self.transform_closure_body(
                    condition,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
                then_block: self.transform_closure_body_block(
                    then_block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
                else_block: else_block.as_ref().map(|b| {
                    self.transform_closure_body_block(b, captures, struct_type_id, self_ref_type)
                }),
            },
            TirStmtKind::While { condition, body } => TirStmtKind::While {
                condition: self.transform_closure_body(
                    condition,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
                body: self.transform_closure_body_block(
                    body,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
            },
            TirStmtKind::Loop { body } => TirStmtKind::Loop {
                body: self.transform_closure_body_block(
                    body,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
            },
            TirStmtKind::Break { label, value } => TirStmtKind::Break {
                label: label.clone(),
                value: value.as_ref().map(|v| {
                    self.transform_closure_body(v, captures, struct_type_id, self_ref_type, span)
                }),
            },
            TirStmtKind::Continue => TirStmtKind::Continue,
            // For other statement types, clone as-is
            other => other.clone(),
        };
        TirStmt::new(kind, stmt.span)
    }

    // ========================================================================
    // Fn-Param Specialization
    // ========================================================================

    /// Generate specialized functions for calls with closure arguments to fn-type parameters.
    ///
    /// This implements WEP Phase 3: when a function takes `fn(A) -> B` and is called with
    /// a closure, we generate a specialized version where:
    /// 1. The fn-type parameter becomes the functor struct type
    /// 2. `IndirectCall` on that parameter becomes `MethodCall` on __call
    fn generate_fn_param_specializations(
        &mut self,
        func_refs: &[Rc<RefCell<TirFunction>>],
        impls: &[crate::tir::TirImpl],
        type_table: &mut TypeTable,
    ) {
        // Build a map from function name to function for quick lookup
        let mut func_by_name: HashMap<String, Rc<RefCell<TirFunction>>> = HashMap::new();
        for func_rc in func_refs {
            let func = func_rc.borrow();
            func_by_name.insert(func.name.clone(), Rc::clone(func_rc));
        }
        for impl_block in impls {
            for method in &impl_block.methods {
                let name = method.name.clone();
                // We can't get an Rc from a TirFunction reference directly
                // For impl methods, we'll handle them separately
                // For now, just process top-level functions
                drop(name);
            }
        }

        // Collect specialization requests by scanning all function bodies
        let mut spec_requests: Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)> = Vec::new();

        // Reset closure counter for this pass
        self.closure_counter = 0;

        for func_rc in func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.collect_fn_param_specs(body, &func_by_name, type_table, &mut spec_requests);
            }
        }

        // Also scan impl methods
        for impl_block in impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.collect_fn_param_specs(
                        body,
                        &func_by_name,
                        type_table,
                        &mut spec_requests,
                    );
                }
            }
        }

        // Generate specialized functions for each unique key
        for (key, callee_rc) in spec_requests {
            if self.fn_param_specializations.contains_key(&key) {
                continue; // Already generated
            }

            // Skip specialization if any fn-param is stored in a struct field
            // This would cause type mismatches since struct fields expect fn(...) not &__Closure_N
            let callee = callee_rc.borrow();
            // Check if this is an instance method (has self parameter)
            // Note: static methods have method_info but no self parameter
            let has_self_param = callee.params.first().is_some_and(|p| p.name == "self");
            let param_offset = u32::from(has_self_param);
            let fn_param_indices: Vec<u32> = key
                .functor_types
                .iter()
                .map(|(arg_idx, _)| arg_idx + param_offset)
                .collect();

            if let Some(body) = &callee.body
                && self.fn_param_stored_in_struct_field(body, &fn_param_indices)
            {
                // Skip this specialization - the closure is stored in a struct field
                continue;
            }
            drop(callee);

            let specialized_name = self.generate_specialized_function(&key, &callee_rc, type_table);
            self.fn_param_specializations.insert(key, specialized_name);
        }
    }

    /// Check if any of the given local indices (fn-params) are used as struct field values.
    /// If so, we can't specialize because the struct field expects fn(...) not &__`Closure_N`.
    fn fn_param_stored_in_struct_field(&self, block: &TirBlock, fn_param_indices: &[u32]) -> bool {
        for stmt in &block.stmts {
            if self.fn_param_in_struct_field_stmt(stmt, fn_param_indices) {
                return true;
            }
        }
        false
    }

    fn fn_param_in_struct_field_stmt(&self, stmt: &TirStmt, fn_param_indices: &[u32]) -> bool {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.fn_param_in_struct_field_expr(expr, fn_param_indices)
            }
            TirStmtKind::Return { value: None }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue => false,
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.fn_param_in_struct_field_expr(condition, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(then_block, fn_param_indices)
                    || else_block
                        .as_ref()
                        .is_some_and(|b| self.fn_param_stored_in_struct_field(b, fn_param_indices))
            }
            TirStmtKind::While { condition, body } => {
                self.fn_param_in_struct_field_expr(condition, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(body, fn_param_indices)
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                init.iter()
                    .any(|s| self.fn_param_in_struct_field_stmt(s, fn_param_indices))
                    || condition
                        .as_ref()
                        .is_some_and(|c| self.fn_param_in_struct_field_expr(c, fn_param_indices))
                    || self.fn_param_stored_in_struct_field(body, fn_param_indices)
                    || update
                        .as_ref()
                        .is_some_and(|u| self.fn_param_in_struct_field_expr(u, fn_param_indices))
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.fn_param_stored_in_struct_field(body, fn_param_indices)
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.fn_param_in_struct_field_expr(iterable, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(body, fn_param_indices)
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(then_block, fn_param_indices)
                    || else_block
                        .as_ref()
                        .is_some_and(|b| self.fn_param_stored_in_struct_field(b, fn_param_indices))
            }
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(body, fn_param_indices)
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                init.iter()
                    .any(|s| self.fn_param_in_struct_field_stmt(s, fn_param_indices))
                    || self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(body, fn_param_indices)
                    || update
                        .as_ref()
                        .is_some_and(|u| self.fn_param_in_struct_field_expr(u, fn_param_indices))
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
        }
    }

    fn fn_param_in_struct_field_expr(&self, expr: &TirExpr, fn_param_indices: &[u32]) -> bool {
        match &expr.kind {
            // Key case: check if fn-param local is used as struct field value
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    // Check if field value is a Local that is one of our fn-params
                    if let TirExprKind::Local { index, .. } = &field.value.kind
                        && fn_param_indices.contains(index)
                    {
                        return true;
                    }
                    // Also recurse into nested expressions
                    if self.fn_param_in_struct_field_expr(&field.value, fn_param_indices) {
                        return true;
                    }
                }
                false
            }
            // Recurse into sub-expressions
            TirExprKind::Binary { left, right, .. } => {
                self.fn_param_in_struct_field_expr(left, fn_param_indices)
                    || self.fn_param_in_struct_field_expr(right, fn_param_indices)
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::OptionSome { value: inner }
            | TirExprKind::Move { value: inner } => {
                self.fn_param_in_struct_field_expr(inner, fn_param_indices)
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::StaticCall { args, .. }
            | TirExprKind::EffectCall { args, .. } => args
                .iter()
                .any(|a| self.fn_param_in_struct_field_expr(a, fn_param_indices)),
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.fn_param_in_struct_field_expr(receiver, fn_param_indices)
                    || args
                        .iter()
                        .any(|a| self.fn_param_in_struct_field_expr(a, fn_param_indices))
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.fn_param_in_struct_field_expr(callee, fn_param_indices)
                    || args
                        .iter()
                        .any(|a| self.fn_param_in_struct_field_expr(a, fn_param_indices))
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.fn_param_in_struct_field_expr(functor, fn_param_indices)
            }
            TirExprKind::Block(block) => {
                self.fn_param_stored_in_struct_field(block, fn_param_indices)
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.fn_param_in_struct_field_expr(condition, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(then_branch, fn_param_indices)
                    || else_branch
                        .as_ref()
                        .is_some_and(|b| self.fn_param_stored_in_struct_field(b, fn_param_indices))
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                elements
                    .iter()
                    .any(|e| self.fn_param_in_struct_field_expr(e, fn_param_indices))
            }
            TirExprKind::Assign { target, value } => {
                self.fn_param_in_struct_field_expr(target, fn_param_indices)
                    || self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            TirExprKind::Index { expr: array, index } => {
                self.fn_param_in_struct_field_expr(array, fn_param_indices)
                    || self.fn_param_in_struct_field_expr(index, fn_param_indices)
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || arms
                        .iter()
                        .any(|arm| self.fn_param_in_struct_field_expr(&arm.body, fn_param_indices))
            }
            TirExprKind::VariantConstruct { payload, .. } => payload
                .as_ref()
                .is_some_and(|p| self.fn_param_in_struct_field_expr(p, fn_param_indices)),
            TirExprKind::LabeledBlock { block, .. } => {
                self.fn_param_stored_in_struct_field(block, fn_param_indices)
            }
            TirExprKind::Closure { body, .. } => {
                self.fn_param_in_struct_field_expr(body, fn_param_indices)
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            // Terminals
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => false,
        }
    }

    /// Collect fn-param specialization requests from a block
    fn collect_fn_param_specs(
        &mut self,
        block: &TirBlock,
        func_by_name: &HashMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
        requests: &mut Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)>,
    ) {
        for stmt in &block.stmts {
            self.collect_fn_param_specs_stmt(stmt, func_by_name, type_table, requests);
        }
    }

    fn collect_fn_param_specs_stmt(
        &mut self,
        stmt: &TirStmt,
        func_by_name: &HashMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
        requests: &mut Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)>,
    ) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.collect_fn_param_specs_expr(expr, func_by_name, type_table, requests);
            }
            TirStmtKind::Return { value: None }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_fn_param_specs_expr(condition, func_by_name, type_table, requests);
                self.collect_fn_param_specs(then_block, func_by_name, type_table, requests);
                if let Some(else_blk) = else_block {
                    self.collect_fn_param_specs(else_blk, func_by_name, type_table, requests);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.collect_fn_param_specs_expr(condition, func_by_name, type_table, requests);
                self.collect_fn_param_specs(body, func_by_name, type_table, requests);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.collect_fn_param_specs_stmt(s, func_by_name, type_table, requests);
                }
                if let Some(cond) = condition {
                    self.collect_fn_param_specs_expr(cond, func_by_name, type_table, requests);
                }
                self.collect_fn_param_specs(body, func_by_name, type_table, requests);
                if let Some(upd) = update {
                    self.collect_fn_param_specs_expr(upd, func_by_name, type_table, requests);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.collect_fn_param_specs(body, func_by_name, type_table, requests);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.collect_fn_param_specs_expr(iterable, func_by_name, type_table, requests);
                self.collect_fn_param_specs(body, func_by_name, type_table, requests);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                self.collect_fn_param_specs(then_block, func_by_name, type_table, requests);
                if let Some(else_blk) = else_block {
                    self.collect_fn_param_specs(else_blk, func_by_name, type_table, requests);
                }
            }
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                self.collect_fn_param_specs(body, func_by_name, type_table, requests);
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                for s in init {
                    self.collect_fn_param_specs_stmt(s, func_by_name, type_table, requests);
                }
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                self.collect_fn_param_specs(body, func_by_name, type_table, requests);
                if let Some(upd) = update {
                    self.collect_fn_param_specs_expr(upd, func_by_name, type_table, requests);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
        }
    }

    fn collect_fn_param_specs_expr(
        &mut self,
        expr: &TirExpr,
        func_by_name: &HashMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
        requests: &mut Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)>,
    ) {
        match &expr.kind {
            TirExprKind::Closure { body, .. } => {
                // Count this closure to keep counter in sync
                self.closure_counter += 1;
                // Recursively scan the body
                self.collect_fn_param_specs_expr(body, func_by_name, type_table, requests);
            }
            // Check for calls with closure arguments
            TirExprKind::Call {
                func,
                args,
                type_args: _,
            } => {
                // Check if this is a call to a known function with closure args
                if let Some(callee_rc) = func_by_name.get(&func.name()) {
                    let callee = callee_rc.borrow();
                    if let Some(key) = self.create_fn_param_spec_key(
                        &callee.name,
                        &callee.params,
                        args,
                        type_table,
                    ) {
                        requests.push((key, Rc::clone(callee_rc)));
                    }
                }
                // Recursively scan args (this will increment counter for any closure args)
                for arg in args {
                    self.collect_fn_param_specs_expr(arg, func_by_name, type_table, requests);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                type_args: _,
            } => {
                // Check if this is a method call with closure args
                if let Some(callee_rc) = func_by_name.get(&func.name()) {
                    let callee = callee_rc.borrow();
                    // Skip self parameter (first param)
                    let params_without_self: Vec<_> =
                        callee.params.iter().skip(1).cloned().collect();
                    if let Some(key) = self.create_fn_param_spec_key(
                        &callee.name,
                        &params_without_self,
                        args,
                        type_table,
                    ) {
                        requests.push((key, Rc::clone(callee_rc)));
                    }
                }
                // Recursively scan receiver first (important for counter sync)
                self.collect_fn_param_specs_expr(receiver, func_by_name, type_table, requests);
                // Then scan args (this will increment counter for any closure args)
                for arg in args {
                    self.collect_fn_param_specs_expr(arg, func_by_name, type_table, requests);
                }
            }
            TirExprKind::StaticCall { func, args } => {
                if let Some(callee_rc) = func_by_name.get(&func.name()) {
                    let callee = callee_rc.borrow();
                    if let Some(key) = self.create_fn_param_spec_key(
                        &callee.name,
                        &callee.params,
                        args,
                        type_table,
                    ) {
                        requests.push((key, Rc::clone(callee_rc)));
                    }
                }
                // Recursively scan args (this will increment counter for any closure args)
                for arg in args {
                    self.collect_fn_param_specs_expr(arg, func_by_name, type_table, requests);
                }
            }
            // Recurse into sub-expressions
            TirExprKind::Binary { left, right, .. } => {
                self.collect_fn_param_specs_expr(left, func_by_name, type_table, requests);
                self.collect_fn_param_specs_expr(right, func_by_name, type_table, requests);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::OptionSome { value: inner }
            | TirExprKind::Move { value: inner } => {
                self.collect_fn_param_specs_expr(inner, func_by_name, type_table, requests);
            }
            TirExprKind::EffectCall { args, .. } => {
                for arg in args {
                    self.collect_fn_param_specs_expr(arg, func_by_name, type_table, requests);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_fn_param_specs_expr(callee, func_by_name, type_table, requests);
                for arg in args {
                    self.collect_fn_param_specs_expr(arg, func_by_name, type_table, requests);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_fn_param_specs_expr(functor, func_by_name, type_table, requests);
            }
            TirExprKind::Block(block) => {
                self.collect_fn_param_specs(block, func_by_name, type_table, requests);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_fn_param_specs_expr(condition, func_by_name, type_table, requests);
                self.collect_fn_param_specs(then_branch, func_by_name, type_table, requests);
                if let Some(else_blk) = else_branch {
                    self.collect_fn_param_specs(else_blk, func_by_name, type_table, requests);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_fn_param_specs_expr(
                        &field.value,
                        func_by_name,
                        type_table,
                        requests,
                    );
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_fn_param_specs_expr(elem, func_by_name, type_table, requests);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_fn_param_specs_expr(target, func_by_name, type_table, requests);
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_fn_param_specs_expr(array, func_by_name, type_table, requests);
                self.collect_fn_param_specs_expr(index, func_by_name, type_table, requests);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                for arm in arms {
                    self.collect_fn_param_specs_expr(&arm.body, func_by_name, type_table, requests);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_fn_param_specs_expr(
                        payload_expr,
                        func_by_name,
                        type_table,
                        requests,
                    );
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_fn_param_specs(block, func_by_name, type_table, requests);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            // Terminals
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
        }
    }

    /// Create a fn-param specialization key if there are closure arguments to fn-type parameters.
    /// Uses the current `closure_counter` to determine functor IDs for closure args.
    fn create_fn_param_spec_key(
        &mut self,
        callee_name: &str,
        params: &[TirParam],
        args: &[TirExpr],
        type_table: &TypeTable,
    ) -> Option<FnParamSpecKey> {
        let mut functor_types = Vec::new();

        // Track the closure counter as we scan args in order
        let mut local_counter = self.closure_counter;

        for (i, (param, arg)) in params.iter().zip(args.iter()).enumerate() {
            // Count closures in this arg to keep counter in sync
            let closure_id = self.count_closures_and_get_first_id(arg, &mut local_counter);

            // Check if param is a function type and arg is a direct closure
            if let crate::tir::ResolvedType::Function { .. } = type_table.get(param.type_id)
                && matches!(&arg.kind, TirExprKind::Closure { .. })
            {
                // closure_id is the ID of this closure (before we counted it)
                if let Some(functor) = self.functor_infos.get(closure_id as usize) {
                    functor_types.push((i as u32, functor.struct_type_id));
                }
            }
        }

        if functor_types.is_empty() {
            return None;
        }

        Some(FnParamSpecKey {
            callee_name: callee_name.to_string(),
            functor_types,
        })
    }

    /// Count closures in an expression and return the ID of the first closure (if any).
    /// Updates the counter in place.
    fn count_closures_and_get_first_id(&self, expr: &TirExpr, counter: &mut u32) -> u32 {
        let first_id = *counter;
        self.count_closures_in_expr(expr, counter);
        first_id
    }

    /// Count closures in an expression, updating the counter.
    fn count_closures_in_expr(&self, expr: &TirExpr, counter: &mut u32) {
        match &expr.kind {
            TirExprKind::Closure { body, .. } => {
                *counter += 1;
                self.count_closures_in_expr(body, counter);
            }
            TirExprKind::Binary { left, right, .. } => {
                self.count_closures_in_expr(left, counter);
                self.count_closures_in_expr(right, counter);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::OptionSome { value: inner }
            | TirExprKind::Move { value: inner } => {
                self.count_closures_in_expr(inner, counter);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::StaticCall { args, .. }
            | TirExprKind::EffectCall { args, .. } => {
                for arg in args {
                    self.count_closures_in_expr(arg, counter);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.count_closures_in_expr(receiver, counter);
                for arg in args {
                    self.count_closures_in_expr(arg, counter);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.count_closures_in_expr(callee, counter);
                for arg in args {
                    self.count_closures_in_expr(arg, counter);
                }
            }
            TirExprKind::Block(block) => {
                self.count_closures_in_block(block, counter);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.count_closures_in_expr(condition, counter);
                self.count_closures_in_block(then_branch, counter);
                if let Some(else_blk) = else_branch {
                    self.count_closures_in_block(else_blk, counter);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.count_closures_in_expr(&field.value, counter);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.count_closures_in_expr(elem, counter);
                }
            }
            TirExprKind::Index { expr: array, index } => {
                self.count_closures_in_expr(array, counter);
                self.count_closures_in_expr(index, counter);
            }
            TirExprKind::Assign { target, value } => {
                self.count_closures_in_expr(target, counter);
                self.count_closures_in_expr(value, counter);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.count_closures_in_expr(scrutinee, counter);
                for arm in arms {
                    self.count_closures_in_expr(&arm.body, counter);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.count_closures_in_expr(payload_expr, counter);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.count_closures_in_block(block, counter);
            }
            // Terminals
            _ => {}
        }
    }

    fn count_closures_in_block(&self, block: &TirBlock, counter: &mut u32) {
        for stmt in &block.stmts {
            self.count_closures_in_stmt(stmt, counter);
        }
    }

    fn count_closures_in_stmt(&self, stmt: &TirStmt, counter: &mut u32) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.count_closures_in_expr(value, counter);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.count_closures_in_expr(expr, counter);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.count_closures_in_expr(condition, counter);
                self.count_closures_in_block(then_block, counter);
                if let Some(else_blk) = else_block {
                    self.count_closures_in_block(else_blk, counter);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.count_closures_in_expr(condition, counter);
                self.count_closures_in_block(body, counter);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.count_closures_in_stmt(s, counter);
                }
                if let Some(cond) = condition {
                    self.count_closures_in_expr(cond, counter);
                }
                self.count_closures_in_block(body, counter);
                if let Some(upd) = update {
                    self.count_closures_in_expr(upd, counter);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.count_closures_in_block(body, counter);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.count_closures_in_expr(iterable, counter);
                self.count_closures_in_block(body, counter);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.count_closures_in_expr(scrutinee, counter);
                self.count_closures_in_block(then_block, counter);
                if let Some(else_blk) = else_block {
                    self.count_closures_in_block(else_blk, counter);
                }
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.count_closures_in_expr(v, counter);
                }
            }
            _ => {}
        }
    }

    /// Generate a specialized function where fn-type params become functor struct types
    fn generate_specialized_function(
        &mut self,
        key: &FnParamSpecKey,
        callee_rc: &Rc<RefCell<TirFunction>>,
        type_table: &mut TypeTable,
    ) -> String {
        let callee = callee_rc.borrow();

        // Build specialized name: callee$__Closure_0$__Closure_1...
        let functor_suffix: String = key
            .functor_types
            .iter()
            .map(|(_, tid)| {
                let name = type_table.type_name(*tid);
                format!("${name}")
            })
            .collect();
        let specialized_name = format!("{}{}", callee.name, functor_suffix);

        // Build map from argument index to functor type
        // Note: key.functor_types contains argument indices (0 = first arg after receiver for methods)
        let arg_to_functor: HashMap<u32, TypeId> = key.functor_types.iter().copied().collect();

        // Determine if this is an instance method (has self parameter)
        // Note: static methods have method_info but no self parameter
        let has_self_param = callee.params.first().is_some_and(|p| p.name == "self");
        let param_offset = u32::from(has_self_param);

        // Clone and modify params
        // For methods: params[0] is self, so argument index i maps to params[i + 1]
        let mut new_params = callee.params.clone();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let param_idx = (*arg_idx + param_offset) as usize;
            if param_idx < new_params.len() {
                new_params[param_idx].type_id = type_table.make_ref(functor_type);
            }
        }

        // Clone and modify local_types (same indexing as params)
        let mut new_local_types = callee.local_types.clone();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let local_idx = (*arg_idx + param_offset) as usize;
            if local_idx < new_local_types.len() {
                new_local_types[local_idx] = type_table.make_ref(functor_type);
            }
        }

        // Build a map from param/local index to functor type for body transformation
        // Inside the function body, locals are referenced by param index
        let local_to_functor: HashMap<u32, TypeId> = arg_to_functor
            .iter()
            .map(|(arg_idx, functor_type)| (arg_idx + param_offset, *functor_type))
            .collect();

        // Clone body and transform IndirectCall to MethodCall for fn-param locals
        let new_body = callee
            .body
            .as_ref()
            .map(|body| self.specialize_function_body(body, &local_to_functor, type_table));

        // Build specialized method name: method<TypeArgs>$__Closure_0
        // The functor suffix goes AFTER type args, so we use full_method_name()
        // and then clear method_type_args to avoid duplication
        let specialized_method_name = if let Some(ref info) = callee.method_info {
            format!("{}{}", info.full_method_name(), functor_suffix)
        } else {
            // Should not happen for method calls
            format!("{}{}", callee.name, functor_suffix)
        };

        // Update method_info with the specialized method name
        // Note: method_type_args is empty because they're now part of method_name
        let specialized_method_info = callee.method_info.as_ref().map(|info| {
            LocalMethodName {
                struct_name: info.struct_name.clone(),
                base_struct_name: info.base_struct_name.clone(),
                trait_name: info.trait_name.clone(),
                method_name: specialized_method_name.clone(),
                method_type_args: Vec::new(), // Type args are now in method_name
            }
        });

        let specialized_func = TirFunction {
            name: specialized_name.clone(),
            is_pub: false, // Specialized functions are always private
            type_params: callee.type_params.clone(),
            impl_type_params: callee.impl_type_params.clone(),
            monomorph_info: callee.monomorph_info.clone(),
            method_info: specialized_method_info,
            params: new_params,
            return_type: callee.return_type,
            effects: callee.effects.clone(),
            body: new_body,
            span: callee.span,
            local_count: callee.local_count,
            local_types: new_local_types,
            address_taken_locals: callee.address_taken_locals.clone(),
            needed_copy_types: callee.needed_copy_types.clone(),
        };

        self.generated_functions
            .push(Rc::new(RefCell::new(specialized_func)));
        specialized_name
    }

    /// Specialize a function body by transforming `IndirectCall` to `MethodCall` for fn-param locals
    fn specialize_function_body(
        &self,
        block: &TirBlock,
        param_to_functor: &HashMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TirBlock {
        TirBlock::new(
            block
                .stmts
                .iter()
                .map(|stmt| self.specialize_stmt(stmt, param_to_functor, type_table))
                .collect(),
            block.span,
        )
    }

    fn specialize_stmt(
        &self,
        stmt: &TirStmt,
        param_to_functor: &HashMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TirStmt {
        let kind = match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
            } => TirStmtKind::Let {
                name: name.clone(),
                local_index: *local_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.specialize_expr(value, param_to_functor, type_table),
            },
            TirStmtKind::Expr(expr) => {
                TirStmtKind::Expr(self.specialize_expr(expr, param_to_functor, type_table))
            }
            TirStmtKind::Return { value } => TirStmtKind::Return {
                value: value
                    .as_ref()
                    .map(|e| self.specialize_expr(e, param_to_functor, type_table)),
            },
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => TirStmtKind::If {
                condition: self.specialize_expr(condition, param_to_functor, type_table),
                then_block: self.specialize_function_body(then_block, param_to_functor, type_table),
                else_block: else_block
                    .as_ref()
                    .map(|b| self.specialize_function_body(b, param_to_functor, type_table)),
            },
            TirStmtKind::While { condition, body } => TirStmtKind::While {
                condition: self.specialize_expr(condition, param_to_functor, type_table),
                body: self.specialize_function_body(body, param_to_functor, type_table),
            },
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => TirStmtKind::For {
                init: init
                    .iter()
                    .map(|s| self.specialize_stmt(s, param_to_functor, type_table))
                    .collect(),
                condition: condition
                    .as_ref()
                    .map(|c| self.specialize_expr(c, param_to_functor, type_table)),
                body: self.specialize_function_body(body, param_to_functor, type_table),
                update: update
                    .as_ref()
                    .map(|u| self.specialize_expr(u, param_to_functor, type_table)),
            },
            TirStmtKind::Loop { body } => TirStmtKind::Loop {
                body: self.specialize_function_body(body, param_to_functor, type_table),
            },
            TirStmtKind::ForOf {
                binding_local,
                binding_type,
                is_mut,
                iterable,
                iterable_type,
                body,
            } => TirStmtKind::ForOf {
                binding_local: *binding_local,
                binding_type: *binding_type,
                is_mut: *is_mut,
                iterable: self.specialize_expr(iterable, param_to_functor, type_table),
                iterable_type: *iterable_type,
                body: self.specialize_function_body(body, param_to_functor, type_table),
            },
            TirStmtKind::LabeledBlock { label, block } => TirStmtKind::LabeledBlock {
                label: label.clone(),
                block: self.specialize_function_body(block, param_to_functor, type_table),
            },
            TirStmtKind::Break { label, value } => TirStmtKind::Break {
                label: label.clone(),
                value: value
                    .as_ref()
                    .map(|v| self.specialize_expr(v, param_to_functor, type_table)),
            },
            TirStmtKind::Continue => TirStmtKind::Continue,
            TirStmtKind::IfPattern {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => TirStmtKind::IfPattern {
                scrutinee: self.specialize_expr(scrutinee, param_to_functor, type_table),
                pattern: pattern.clone(),
                then_block: self.specialize_function_body(then_block, param_to_functor, type_table),
                else_block: else_block
                    .as_ref()
                    .map(|b| self.specialize_function_body(b, param_to_functor, type_table)),
            },
            TirStmtKind::WhilePattern {
                scrutinee,
                pattern,
                body,
            } => TirStmtKind::WhilePattern {
                scrutinee: self.specialize_expr(scrutinee, param_to_functor, type_table),
                pattern: pattern.clone(),
                body: self.specialize_function_body(body, param_to_functor, type_table),
            },
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                pattern,
                body,
                update,
            } => TirStmtKind::ForPattern {
                init: init
                    .iter()
                    .map(|s| self.specialize_stmt(s, param_to_functor, type_table))
                    .collect(),
                scrutinee: self.specialize_expr(scrutinee, param_to_functor, type_table),
                pattern: pattern.clone(),
                body: self.specialize_function_body(body, param_to_functor, type_table),
                update: update
                    .as_ref()
                    .map(|u| self.specialize_expr(u, param_to_functor, type_table)),
            },
            TirStmtKind::LetPattern {
                pattern,
                is_mut,
                value,
            } => TirStmtKind::LetPattern {
                pattern: pattern.clone(),
                is_mut: *is_mut,
                value: self.specialize_expr(value, param_to_functor, type_table),
            },
        };
        TirStmt::new(kind, stmt.span)
    }

    fn specialize_expr(
        &self,
        expr: &TirExpr,
        param_to_functor: &HashMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TirExpr {
        match &expr.kind {
            // Transform IndirectCall on a fn-param local to MethodCall on __call
            TirExprKind::IndirectCall { callee, args } => {
                // Check if callee is a Local that maps to a fn-param with functor type
                if let TirExprKind::Local { index, .. } = &callee.kind
                    && let Some(&functor_type) = param_to_functor.get(index)
                {
                    // Get the functor info to find the __call method name
                    if let Some(functor) = self
                        .functor_infos
                        .iter()
                        .find(|f| f.struct_type_id == functor_type)
                    {
                        let call_method_name = format!("{}::__call", functor.struct_name);

                        // Transform to MethodCall
                        let new_callee = self.specialize_expr(callee, param_to_functor, type_table);
                        let new_args: Vec<_> = args
                            .iter()
                            .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                            .collect();

                        // Build method_info for the __call method
                        let call_method_info = LocalMethodName::new(
                            functor.struct_name.clone(), // __Closure_N
                            None,                        // no trait
                            "__call".to_string(),        // just the method name
                        );

                        // Use the call_method_name for External ref since codegen expects it
                        return TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(new_callee),
                                func: FunctionRef::External {
                                    module_source: self.module_source.clone(),
                                    name: call_method_name,
                                    monomorph_info: None,
                                    method_info: Some(call_method_info),
                                },
                                args: new_args,
                                type_args: Vec::new(),
                            },
                            expr.type_id,
                            expr.span,
                        );
                    }
                }
                // Not a fn-param local, recurse normally
                TirExpr::new(
                    TirExprKind::IndirectCall {
                        callee: Box::new(self.specialize_expr(
                            callee,
                            param_to_functor,
                            type_table,
                        )),
                        args: args
                            .iter()
                            .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                            .collect(),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
            } => TirExpr::new(
                TirExprKind::ClosureToCanonical {
                    functor: Box::new(self.specialize_expr(functor, param_to_functor, type_table)),
                    functor_id: *functor_id,
                    target_fn_type: *target_fn_type,
                },
                expr.type_id,
                expr.span,
            ),
            // Recurse into sub-expressions
            TirExprKind::Binary { left, op, right } => TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(self.specialize_expr(left, param_to_functor, type_table)),
                    op: *op,
                    right: Box::new(self.specialize_expr(right, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Unary { op, expr: inner } => TirExpr::new(
                TirExprKind::Unary {
                    op: *op,
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Call {
                func,
                args,
                type_args,
            } => TirExpr::new(
                TirExprKind::Call {
                    func: func.clone(),
                    args: args
                        .iter()
                        .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                        .collect(),
                    type_args: type_args.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                type_args,
            } => TirExpr::new(
                TirExprKind::MethodCall {
                    receiver: Box::new(self.specialize_expr(
                        receiver,
                        param_to_functor,
                        type_table,
                    )),
                    func: func.clone(),
                    args: args
                        .iter()
                        .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                        .collect(),
                    type_args: type_args.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::StaticCall { func, args } => TirExpr::new(
                TirExprKind::StaticCall {
                    func: func.clone(),
                    args: args
                        .iter()
                        .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::EffectCall {
                effect_name,
                op_name,
                args,
                cm_convention,
                cm_local_name,
            } => TirExpr::new(
                TirExprKind::EffectCall {
                    effect_name: effect_name.clone(),
                    op_name: op_name.clone(),
                    args: args
                        .iter()
                        .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                        .collect(),
                    cm_convention: cm_convention.clone(),
                    cm_local_name: cm_local_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Block(block) => TirExpr::new(
                TirExprKind::Block(self.specialize_function_body(
                    block,
                    param_to_functor,
                    type_table,
                )),
                expr.type_id,
                expr.span,
            ),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(self.specialize_expr(
                        condition,
                        param_to_functor,
                        type_table,
                    )),
                    then_branch: self.specialize_function_body(
                        then_branch,
                        param_to_functor,
                        type_table,
                    ),
                    else_branch: else_branch
                        .as_ref()
                        .map(|b| self.specialize_function_body(b, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    target_type: *target_type,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                field_name,
            } => TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Index { expr: array, index } => TirExpr::new(
                TirExprKind::Index {
                    expr: Box::new(self.specialize_expr(array, param_to_functor, type_table)),
                    index: Box::new(self.specialize_expr(index, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Assign { target, value } => TirExpr::new(
                TirExprKind::Assign {
                    target: Box::new(self.specialize_expr(target, param_to_functor, type_table)),
                    value: Box::new(self.specialize_expr(value, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => TirExpr::new(
                TirExprKind::Match {
                    expr: Box::new(self.specialize_expr(scrutinee, param_to_functor, type_table)),
                    arms: arms
                        .iter()
                        .map(|arm| crate::tir::TirMatchArm {
                            pattern: arm.pattern.clone(),
                            body: self.specialize_expr(&arm.body, param_to_functor, type_table),
                            span: arm.span,
                        })
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Closure {
                params,
                body,
                captures,
                functor_id,
            } => TirExpr::new(
                TirExprKind::Closure {
                    params: params.clone(),
                    body: Box::new(self.specialize_expr(body, param_to_functor, type_table)),
                    captures: captures.clone(),
                    functor_id: *functor_id,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type: *struct_type,
                    struct_name: struct_name.clone(),
                    fields: fields
                        .iter()
                        .map(|f| crate::tir::TirStructField {
                            name: f.name.clone(),
                            value: self.specialize_expr(&f.value, param_to_functor, type_table),
                            field_index: f.field_index,
                        })
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::ArrayLiteral { elements } => TirExpr::new(
                TirExprKind::ArrayLiteral {
                    elements: elements
                        .iter()
                        .map(|e| self.specialize_expr(e, param_to_functor, type_table))
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::TupleLiteral { elements } => TirExpr::new(
                TirExprKind::TupleLiteral {
                    elements: elements
                        .iter()
                        .map(|e| self.specialize_expr(e, param_to_functor, type_table))
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::OptionSome { value } => TirExpr::new(
                TirExprKind::OptionSome {
                    value: Box::new(self.specialize_expr(value, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: *variant_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                    payload: payload
                        .as_ref()
                        .map(|p| Box::new(self.specialize_expr(p, param_to_functor, type_table))),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Move { value } => TirExpr::new(
                TirExprKind::Move {
                    value: Box::new(self.specialize_expr(value, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => TirExpr::new(
                TirExprKind::LabeledBlock {
                    label: label.clone(),
                    block: self.specialize_function_body(block, param_to_functor, type_table),
                    result_type: *result_type,
                },
                expr.type_id,
                expr.span,
            ),
            // Terminals - clone as-is
            TirExprKind::Local { index, name } => {
                // Update type if this local is a fn-param with functor type
                let type_id = if let Some(&functor_type) = param_to_functor.get(index) {
                    type_table.make_ref(functor_type)
                } else {
                    expr.type_id
                };
                TirExpr::new(
                    TirExprKind::Local {
                        index: *index,
                        name: name.clone(),
                    },
                    type_id,
                    expr.span,
                )
            }
            TirExprKind::IntLiteral { value, repr } => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: *value,
                    repr: repr.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::FloatLiteral { value, repr } => TirExpr::new(
                TirExprKind::FloatLiteral {
                    value: *value,
                    repr: repr.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::BoolLiteral(v) => {
                TirExpr::new(TirExprKind::BoolLiteral(*v), expr.type_id, expr.span)
            }
            TirExprKind::CharLiteral(c) => {
                TirExpr::new(TirExprKind::CharLiteral(*c), expr.type_id, expr.span)
            }
            TirExprKind::StringLiteral(s) => TirExpr::new(
                TirExprKind::StringLiteral(s.clone()),
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Null => TirExpr::new(TirExprKind::Null, expr.type_id, expr.span),
            TirExprKind::Unit => TirExpr::new(TirExprKind::Unit, expr.type_id, expr.span),
            TirExprKind::Global {
                name,
                module_source,
            } => TirExpr::new(
                TirExprKind::Global {
                    name: name.clone(),
                    module_source: module_source.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::GlobalVarGet {
                name,
                module_source,
            } => TirExpr::new(
                TirExprKind::GlobalVarGet {
                    name: name.clone(),
                    module_source: module_source.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::GlobalVarSet {
                name,
                module_source,
                value,
            } => TirExpr::new(
                TirExprKind::GlobalVarSet {
                    name: name.clone(),
                    module_source: module_source.clone(),
                    value: Box::new(self.specialize_expr(value, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Capture { index, name } => TirExpr::new(
                TirExprKind::Capture {
                    index: *index,
                    name: name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::EnumConstruct {
                enum_type,
                case_index,
                case_name,
            } => TirExpr::new(
                TirExprKind::EnumConstruct {
                    enum_type: *enum_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
        }
    }

    // ========================================================================
    // Second Pass: Transform Closures and IndirectCalls
    // ========================================================================

    fn transform_block(&mut self, block: &mut TirBlock, type_table: &mut TypeTable) {
        for stmt in &mut block.stmts {
            self.transform_stmt(stmt, type_table);
        }
    }

    fn transform_stmt(&mut self, stmt: &mut TirStmt, type_table: &mut TypeTable) {
        match &mut stmt.kind {
            TirStmtKind::Let {
                local_index,
                value,
                type_id,
                ..
            } => {
                // Track if this local stores a closure
                let was_closure = matches!(value.kind, TirExprKind::Closure { .. });
                let closure_id = if was_closure {
                    let id = self.closure_counter;
                    self.local_to_closure.insert(*local_index, id);
                    Some(id)
                } else {
                    None
                };

                self.transform_expr(value, type_table);

                // Update the Let statement's type_id for specializable closures
                // (non-specializable closures keep their fn(...) type for ClosureToCanonical)
                if let Some(id) = closure_id
                    && self.specializable.contains(&id)
                    && let Some(functor) = self.functor_infos.get(id as usize)
                {
                    *type_id = functor.ref_type_id;
                }
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.transform_expr(expr, type_table);
            }
            TirStmtKind::Return { value: None }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.transform_expr(condition, type_table);
                self.transform_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.transform_block(else_blk, type_table);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.transform_expr(condition, type_table);
                self.transform_block(body, type_table);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.transform_stmt(s, type_table);
                }
                if let Some(cond) = condition {
                    self.transform_expr(cond, type_table);
                }
                self.transform_block(body, type_table);
                if let Some(upd) = update {
                    self.transform_expr(upd, type_table);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.transform_block(body, type_table);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.transform_expr(iterable, type_table);
                self.transform_block(body, type_table);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.transform_expr(scrutinee, type_table);
                self.transform_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.transform_block(else_blk, type_table);
                }
            }
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                self.transform_expr(scrutinee, type_table);
                self.transform_block(body, type_table);
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                for s in init {
                    self.transform_stmt(s, type_table);
                }
                self.transform_expr(scrutinee, type_table);
                self.transform_block(body, type_table);
                if let Some(upd) = update {
                    self.transform_expr(upd, type_table);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.transform_expr(value, type_table);
            }
        }
    }

    fn transform_expr(&mut self, expr: &mut TirExpr, type_table: &mut TypeTable) {
        match &mut expr.kind {
            TirExprKind::Closure {
                params: _,
                body,
                captures,
                functor_id,
            } => {
                // Get the current closure ID
                let closure_id = self.closure_counter;
                self.closure_counter += 1;

                // Transform nested closures in the body
                self.transform_expr(body, type_table);

                // Specializable closures: transform to StructLiteral (stored in local, called directly)
                // Non-specializable closures: set functor_id and leave as Closure for fn-param specialization
                // The try_transform_fn_param_call will handle specialization. Any remaining
                // Closure nodes after that are transformed to ClosureToCanonical in a final pass.
                if self.specializable.contains(&closure_id)
                    && let Some(functor) = self.functor_infos.get(closure_id as usize)
                {
                    // Transform Closure to StructLiteral
                    let struct_name = functor.struct_name.clone();

                    // Build field expressions from captures
                    let fields: Vec<crate::tir::TirStructField> = captures
                        .iter()
                        .enumerate()
                        .map(|(i, cap)| crate::tir::TirStructField {
                            name: format!("__capture_{i}"),
                            value: TirExpr::new(
                                TirExprKind::Local {
                                    index: cap.outer_index,
                                    name: cap.name.clone(),
                                },
                                cap.type_id,
                                expr.span,
                            ),
                            field_index: i as u32,
                        })
                        .collect();

                    // Replace with StructLiteral
                    // struct_type uses bare struct type for codegen
                    // type_id uses ref type because functors are reference types
                    expr.kind = TirExprKind::StructLiteral {
                        struct_type: functor.struct_type_id,
                        struct_name,
                        fields,
                    };
                    expr.type_id = functor.ref_type_id;
                } else {
                    // For non-specializable closures (passed as arguments), set the functor_id
                    // so that try_transform_fn_param_call can look up the corresponding ClosureFunctor
                    *functor_id = Some(closure_id);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                // Transform callee and args first
                self.transform_expr(callee, type_table);
                for arg in &mut *args {
                    self.transform_expr(arg, type_table);
                }

                // Check if callee is a local that stores a safe-to-transform closure
                if let TirExprKind::Local { index, .. } = &callee.kind
                    && let Some(closure_id) = self.local_to_closure.get(index)
                    && self.specializable.contains(closure_id)
                    && let Some(functor) = self.functor_infos.get(*closure_id as usize)
                {
                    // Transform IndirectCall to MethodCall
                    // Update callee's type to the functor ref type (functors are reference types)
                    let mut callee_owned = std::mem::replace(
                        callee.as_mut(),
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                    );
                    callee_owned.type_id = functor.ref_type_id;

                    // Get return type from the __call method
                    let return_type = functor.call_method.borrow().return_type;

                    // Replace with MethodCall using FunctionRef::Resolved
                    let args_owned = std::mem::take(args);
                    expr.kind = TirExprKind::MethodCall {
                        receiver: Box::new(callee_owned),
                        func: crate::tir::FunctionRef::Resolved {
                            func: Rc::clone(&functor.call_method),
                            module_source: self.module_source.clone(),
                        },
                        type_args: Vec::new(),
                        args: args_owned,
                    };
                    expr.type_id = return_type;
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.transform_expr(functor, type_table);
            }
            // Recursive cases - transform all sub-expressions
            TirExprKind::Binary { left, right, .. } => {
                self.transform_expr(left, type_table);
                self.transform_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                self.transform_expr(inner, type_table);
            }
            TirExprKind::Call { func, args, .. } | TirExprKind::StaticCall { func, args } => {
                for arg in &mut *args {
                    self.transform_expr(arg, type_table);
                }
                // Check if this call has closure arguments that need fn-param specialization
                self.try_transform_fn_param_call(func, args, type_table);
            }
            TirExprKind::EffectCall { args, .. } => {
                for arg in args {
                    self.transform_expr(arg, type_table);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                type_args: _,
            } => {
                self.transform_expr(receiver, type_table);
                for arg in &mut *args {
                    self.transform_expr(arg, type_table);
                }

                // Check if this call has closure arguments that need fn-param specialization
                self.try_transform_fn_param_call(func, args, type_table);
            }
            TirExprKind::Block(block) => {
                self.transform_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.transform_expr(condition, type_table);
                self.transform_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.transform_block(else_blk, type_table);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.transform_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.transform_expr(elem, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.transform_expr(target, type_table);
                self.transform_expr(value, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.transform_expr(array, type_table);
                self.transform_expr(index, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.transform_expr(scrutinee, type_table);
                for arm in arms {
                    self.transform_expr(&mut arm.body, type_table);
                }
            }
            TirExprKind::OptionSome { value } | TirExprKind::Move { value } => {
                self.transform_expr(value, type_table);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.transform_expr(payload_expr, type_table);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.transform_block(block, type_table);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.transform_expr(value, type_table);
            }
            // Terminals - nothing to transform
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
        }
    }

    /// Try to transform a call with closure arguments to use a specialized function.
    /// This handles fn-param monomorphization: when a closure is passed to a fn-type parameter,
    /// we transform the closure to a struct literal and change the call to use the specialized function.
    fn try_transform_fn_param_call(
        &self,
        func: &mut FunctionRef,
        args: &mut [TirExpr],
        type_table: &mut TypeTable,
    ) {
        // Collect closure args that have functor_id (meaning they were passed as fn-type params)
        let mut functor_types = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if let TirExprKind::Closure {
                functor_id: Some(id),
                ..
            } = &arg.kind
                && let Some(functor) = self.functor_infos.get(*id as usize)
            {
                functor_types.push((i as u32, functor.struct_type_id));
            }
        }

        if functor_types.is_empty() {
            return;
        }

        // Build spec key and look up specialized function
        let key = FnParamSpecKey {
            callee_name: func.name(),
            functor_types: functor_types.clone(),
        };

        let Some(specialized_name) = self.fn_param_specializations.get(&key) else {
            return;
        };

        // Transform closure args to struct literals
        for (arg_idx, _functor_type_id) in &functor_types {
            let arg = &mut args[*arg_idx as usize];
            if let TirExprKind::Closure {
                captures,
                functor_id: Some(closure_id),
                ..
            } = &arg.kind
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
            {
                // Build struct fields from captures
                let fields: Vec<crate::tir::TirStructField> = captures
                    .iter()
                    .enumerate()
                    .map(|(i, cap)| crate::tir::TirStructField {
                        name: format!("__capture_{i}"),
                        value: TirExpr::new(
                            TirExprKind::Local {
                                index: cap.outer_index,
                                name: cap.name.clone(),
                            },
                            cap.type_id,
                            arg.span,
                        ),
                        field_index: i as u32,
                    })
                    .collect();

                // Transform to struct literal (functors are reference types, no Move needed)
                arg.kind = TirExprKind::StructLiteral {
                    struct_type: functor.struct_type_id,
                    struct_name: functor.struct_name.clone(),
                    fields,
                };
                arg.type_id = functor.ref_type_id;
            }
        }

        // Build the functor suffix for the specialized method name
        let functor_suffix: String = functor_types
            .iter()
            .map(|(_, tid)| {
                let name = type_table.type_name(*tid);
                format!("${name}")
            })
            .collect();

        // Build specialized method_info with the specialized method name
        // The functor suffix goes AFTER type args, so we use full_method_name()
        // and then clear method_type_args to avoid duplication
        let specialized_method_info = func.method_info().map(|info| {
            LocalMethodName {
                struct_name: info.struct_name.clone(),
                base_struct_name: info.base_struct_name.clone(),
                trait_name: info.trait_name.clone(),
                method_name: format!("{}{}", info.full_method_name(), functor_suffix),
                method_type_args: Vec::new(), // Type args are now in method_name
            }
        });

        // Update the function reference to use the specialized name
        *func = FunctionRef::External {
            module_source: self.module_source.clone(),
            name: specialized_name.clone(),
            monomorph_info: None,
            method_info: specialized_method_info,
        };
    }

    /// Transform remaining Closure nodes to `ClosureToCanonical`.
    /// This is called after fn-param specialization has had a chance to transform closures.
    /// Any Closure nodes still remaining are those where specialization was skipped
    /// (e.g., fn-param stored in struct field).
    fn transform_remaining_closures_block(&self, block: &mut TirBlock) {
        for stmt in &mut block.stmts {
            self.transform_remaining_closures_stmt(stmt);
        }
    }

    fn transform_remaining_closures_stmt(&self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.transform_remaining_closures_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.transform_remaining_closures_expr(expr);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                self.transform_remaining_closures_expr(expr);
            }
            TirStmtKind::Return { value: None }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.transform_remaining_closures_expr(condition);
                self.transform_remaining_closures_block(then_block);
                if let Some(else_blk) = else_block {
                    self.transform_remaining_closures_block(else_blk);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.transform_remaining_closures_expr(condition);
                self.transform_remaining_closures_block(body);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.transform_remaining_closures_stmt(s);
                }
                if let Some(c) = condition {
                    self.transform_remaining_closures_expr(c);
                }
                self.transform_remaining_closures_block(body);
                if let Some(u) = update {
                    self.transform_remaining_closures_expr(u);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.transform_remaining_closures_block(body);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.transform_remaining_closures_expr(iterable);
                self.transform_remaining_closures_block(body);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.transform_remaining_closures_expr(scrutinee);
                self.transform_remaining_closures_block(then_block);
                if let Some(else_blk) = else_block {
                    self.transform_remaining_closures_block(else_blk);
                }
            }
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                self.transform_remaining_closures_expr(scrutinee);
                self.transform_remaining_closures_block(body);
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                for s in init {
                    self.transform_remaining_closures_stmt(s);
                }
                self.transform_remaining_closures_expr(scrutinee);
                self.transform_remaining_closures_block(body);
                if let Some(u) = update {
                    self.transform_remaining_closures_expr(u);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.transform_remaining_closures_expr(value);
            }
        }
    }

    fn transform_remaining_closures_expr(&self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Closure {
                captures,
                functor_id: Some(closure_id),
                body,
                ..
            } => {
                // Transform nested closures first
                self.transform_remaining_closures_expr(body);

                // This closure wasn't specialized, transform to ClosureToCanonical
                if let Some(functor) = self.functor_infos.get(*closure_id as usize) {
                    let struct_name = functor.struct_name.clone();

                    // Build field expressions from captures
                    let fields: Vec<crate::tir::TirStructField> = captures
                        .iter()
                        .enumerate()
                        .map(|(i, cap)| crate::tir::TirStructField {
                            name: format!("__capture_{i}"),
                            value: TirExpr::new(
                                TirExprKind::Local {
                                    index: cap.outer_index,
                                    name: cap.name.clone(),
                                },
                                cap.type_id,
                                expr.span,
                            ),
                            field_index: i as u32,
                        })
                        .collect();

                    // Build the StructLiteral (functors are reference types)
                    let struct_literal = TirExpr::new(
                        TirExprKind::StructLiteral {
                            struct_type: functor.struct_type_id,
                            struct_name,
                            fields,
                        },
                        functor.ref_type_id,
                        expr.span,
                    );

                    // Wrap in ClosureToCanonical
                    let target_fn_type = expr.type_id; // Original function type
                    expr.kind = TirExprKind::ClosureToCanonical {
                        functor: Box::new(struct_literal),
                        functor_id: *closure_id,
                        target_fn_type,
                    };
                    // Keep original function type for type compatibility
                }
            }
            // Recurse into all expression kinds
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.transform_remaining_closures_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.transform_remaining_closures_expr(receiver);
                for arg in args {
                    self.transform_remaining_closures_expr(arg);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.transform_remaining_closures_expr(left);
                self.transform_remaining_closures_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Move { value: inner } => {
                self.transform_remaining_closures_expr(inner);
            }
            TirExprKind::Assign { target, value } => {
                self.transform_remaining_closures_expr(target);
                self.transform_remaining_closures_expr(value);
            }
            TirExprKind::Index { expr: arr, index } => {
                self.transform_remaining_closures_expr(arr);
                self.transform_remaining_closures_expr(index);
            }
            TirExprKind::Block(block) => {
                self.transform_remaining_closures_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.transform_remaining_closures_expr(condition);
                self.transform_remaining_closures_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.transform_remaining_closures_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.transform_remaining_closures_expr(&mut field.value);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.transform_remaining_closures_expr(elem);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.transform_remaining_closures_expr(callee);
                for arg in args {
                    self.transform_remaining_closures_expr(arg);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.transform_remaining_closures_expr(scrutinee);
                for arm in arms {
                    self.transform_remaining_closures_expr(&mut arm.body);
                }
            }
            TirExprKind::OptionSome { value } => {
                self.transform_remaining_closures_expr(value);
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                self.transform_remaining_closures_expr(p);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.transform_remaining_closures_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.transform_remaining_closures_expr(value);
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.transform_remaining_closures_expr(functor);
            }
            TirExprKind::Closure {
                body,
                functor_id: None,
                ..
            } => {
                // Closure without functor_id - just recurse into body
                self.transform_remaining_closures_expr(body);
            }
            // Leaf nodes - nothing to recurse into
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::VariantConstruct { payload: None, .. } => {}
        }
    }
}

// ============================================================================
// String Literal Collection
// ============================================================================

/// Collects all string literals from a TIR module for the data section,
/// tracking which function each string comes from for DCE
struct StringCollector {
    strings: Vec<String>,
    /// Map of function name → strings in that function (for DCE filtering)
    function_strings: HashMap<String, Vec<String>>,
    /// Current function being collected (for tracking)
    current_function: Option<String>,
}

impl StringCollector {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            function_strings: HashMap::new(),
            current_function: None,
        }
    }

    fn into_results(self) -> (Vec<String>, HashMap<String, Vec<String>>) {
        (self.strings, self.function_strings)
    }

    fn add_string(&mut self, s: String) {
        if !self.strings.contains(&s) {
            self.strings.push(s.clone());
        }
        // Also track which function this string belongs to
        if let Some(func_name) = &self.current_function {
            let func_strings = self.function_strings.entry(func_name.clone()).or_default();
            if !func_strings.contains(&s) {
                func_strings.push(s);
            }
        }
    }

    fn collect_module(&mut self, module: &TirModule) {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.current_function = Some(func.name.clone());
                self.collect_block(body);
                self.current_function = None;
            }
        }
        // Also collect from trait impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.current_function = Some(method.name.clone());
                    self.collect_block(body);
                    self.current_function = None;
                }
            }
        }
    }

    fn collect_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.collect_stmt(stmt);
        }
    }

    fn collect_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.collect_expr(expr);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.collect_expr(expr);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_block(else_blk);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.collect_expr(condition);
                self.collect_block(body);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.collect_stmt(s);
                }
                if let Some(cond) = condition {
                    self.collect_expr(cond);
                }
                self.collect_block(body);
                if let Some(upd) = update {
                    self.collect_expr(upd);
                }
            }
            TirStmtKind::Loop { body } => {
                self.collect_block(body);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.collect_expr(iterable);
                self.collect_block(body);
            }
            TirStmtKind::Break { .. } | TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.collect_block(block);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_expr(scrutinee);
                self.collect_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_block(else_blk);
                }
            }
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                self.collect_expr(scrutinee);
                self.collect_block(body);
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                for s in init {
                    self.collect_stmt(s);
                }
                self.collect_expr(scrutinee);
                self.collect_block(body);
                if let Some(upd) = update {
                    self.collect_expr(upd);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.collect_expr(value);
            }
        }
    }

    fn collect_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::StringLiteral(s) => {
                self.add_string(s.clone());
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_expr(left);
                self.collect_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.collect_expr(receiver);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::Block(block) => {
                self.collect_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.collect_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_expr(&field.value);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_expr(elem);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_expr(target);
                self.collect_expr(value);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_expr(array);
                self.collect_expr(index);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_expr(scrutinee);
                for arm in arms {
                    self.collect_expr(&arm.body);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.collect_expr(body);
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_expr(callee);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_expr(functor);
            }
            TirExprKind::OptionSome { value } => {
                self.collect_expr(value);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_expr(payload_expr);
                }
            }
            TirExprKind::Move { value } => {
                self.collect_expr(value);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_expr(value);
            }
            // Literals and simple expressions don't contain strings
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lower_passthrough() {
        let module = TirModule::new(ModuleSource::Local {
            path: "test".to_string(),
        });
        let lowered = lower(module);
        assert_eq!(
            lowered.module_source,
            ModuleSource::Local {
                path: "test".to_string()
            }
        );
    }

    #[test]
    fn test_string_collector_empty() {
        let module = TirModule::new(ModuleSource::Local {
            path: "test".to_string(),
        });
        let lowered = lower(module);
        assert!(lowered.string_literals.is_empty());
    }
}
