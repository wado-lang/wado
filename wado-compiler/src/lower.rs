//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - Boxing lowering (transform `&primitive` / `&mut primitive` to `Box<T>` struct operations)
//! - String literal collection (for data section)
//! - Closure lowering (transform closures to functor structs with `__call` methods)
//! - i128/u128 match pattern lowering (convert to if-else chains)
//!
//! Note: All loop constructs are desugared at the AST level in desugar.rs.
//! Monomorphization has been moved to a separate phase (see `monomorphize.rs`).

use std::cell::RefCell;
use std::collections::VecDeque;

use indexmap::IndexSet;
use std::fmt::Write as _;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::name::{LocalMethodName, MethodName, ModuleSource, mangle_generic_name};
use crate::project::Project;
use crate::tir::FunctionRef;
use crate::tir::{
    ClosureFunctor, InlineHint, MonomorphInfo, PrimitiveType, ResolvedType, TirBinaryOp, TirBlock,
    TirCapture, TirExpr, TirExprKind, TirField, TirFunction, TirGlobal, TirLiteralPattern,
    TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::token::Span;

/// Lower a TIR module
///
/// Performs:
/// 1. Global variable initialization lowering (extract non-constant initializers)
/// 2. Closure lowering (transform closures to functor structs with `__call` methods)
/// 3. String literal collection for the data section
///
/// Note: All loop constructs are desugared at the AST level in desugar.rs.
pub fn lower(module: TirModule) -> TirModule {
    let modules = IndexMap::from([(module.module_source.clone(), module)]);
    let mut modules = lower_modules_indexed(modules);
    modules.pop().unwrap().1
}

/// Lower a TIR module with access to variants from all modules in the project.
///
/// `global_variant_map` provides variant case info from other modules,
/// enabling pattern matching on imported variants (e.g., `if let Greater = ord`
/// where `Ordering` is defined in a different module).
/// Run pre-boxing per-module lowering passes.
fn lower_pre_boxing(
    module: &mut TirModule,
    global_variant_map: &IndexMap<String, Vec<(String, u32)>>,
) {
    // Phase 1: Lower i128/u128 match patterns to if-else chains
    lower_wide_int_match_patterns(module);

    // Phase 1.5: Lower patterns (LetPattern, IfPattern) to explicit Let statements
    lower_patterns(module, global_variant_map);

    // Phase 2: Lower global variable initializers
    lower_global_initializers(module);
}

/// Run post-boxing per-module lowering passes.
fn lower_post_boxing(module: &mut TirModule) {
    // Phase 3: Lower closures to functor structs
    let mut closure_lowerer = ClosureLowerer::new(&module.module_source);
    closure_lowerer.lower_module(module);

    // Phase 3b: Collect string literals and their function mappings
    let mut collector = StringCollector::new();
    collector.collect_module(module);
    let (strings, function_strings, function_method_info) = collector.into_results();
    module.string_literals = strings;
    module.function_strings = function_strings;
    module.function_method_info = function_method_info;
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
/// Builds a global variant map from all modules so that pattern matching works
/// on imported variants, then applies lowering to each module.
pub fn lower_modules_indexed(
    modules: IndexMap<ModuleSource, TirModule>,
) -> IndexMap<ModuleSource, TirModule> {
    // Build a global variant map from ALL modules so that cross-module pattern
    // matching works (e.g., `if let Greater = ord` where Ordering is from another module)
    let mut global_variant_map: IndexMap<String, Vec<(String, u32)>> = IndexMap::new();
    for module in modules.values() {
        for variant in &module.variants {
            let cases: Vec<(String, u32)> = variant
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index))
                .collect();
            global_variant_map.insert(variant.name.clone(), cases);
        }
    }

    let mut modules: IndexMap<ModuleSource, TirModule> = modules
        .into_iter()
        .map(|(source, mut module)| {
            lower_pre_boxing(&mut module, &global_variant_map);
            (source, module)
        })
        .collect();

    // Phase 2: Lower boxing across ALL modules with a single BoxLowerer.
    // All modules share the same TypeTable, so box type creation and type
    // rewriting must happen once. The BoxLowerer scans the shared type table,
    // creates Box<T> struct types, rewrites Ref/MutRef types, then transforms
    // expressions in each module's functions.
    {
        let mut box_lowerer = BoxLowerer::new();

        // Build struct fields map and variant names from all modules
        for module in modules.values() {
            for s in &module.structs {
                box_lowerer.struct_fields_map.insert(
                    (s.name.clone(), module.module_source.clone()),
                    s.fields.clone(),
                );
            }
            for v in &module.variants {
                box_lowerer.variant_names.insert(v.name.clone());
            }
        }

        // Use any module's type_table (they all share the same Rc<RefCell<TypeTable>>)
        if let Some(first_module) = modules.values().next() {
            let mut type_table = first_module.type_table.borrow_mut();
            box_lowerer.create_needed_box_types(&mut type_table);
            box_lowerer.rewrite_types(&mut type_table);
        }

        // Transform expressions per module
        for module in modules.values_mut() {
            box_lowerer.lower_module_exprs(module);
        }

        // Inject generated Box structs into core:internal module (where they logically live).
        // Falls back to entry module if core:internal doesn't exist (e.g., single-module tests).
        if !box_lowerer.generated_structs.is_empty() {
            let internal_source = ModuleSource::internal();
            let has_internal = modules.contains_key(&internal_source);
            let target_module = if has_internal {
                modules.get_mut(&internal_source).unwrap()
            } else {
                modules.values_mut().next().unwrap()
            };
            target_module
                .structs
                .append(&mut box_lowerer.generated_structs);
        }
    }

    for module in modules.values_mut() {
        lower_post_boxing(module);
    }

    modules
}

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
            value: i64_value.cast_unsigned(),
            repr: value.to_string(),
        },
        TypeTable::I64,
        span,
    );
    let method_info = LocalMethodName::new("i128".to_string(), None, "from_i64".to_string());
    TirExpr::new(
        TirExprKind::StaticCall {
            func: FunctionRef::External {
                module_source: ModuleSource::int128(),
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
                module_source: ModuleSource::int128(),
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
                module_source: ModuleSource::int128(),
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
                module_source: ModuleSource::int128(),
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
        TirStmtKind::Break { .. }
        | TirStmtKind::Continue
        | TirStmtKind::Return { value: None }
        | TirStmtKind::Loop { .. }
        | TirStmtKind::LabeledBlock { .. }
        | TirStmtKind::LetPattern { .. } => {}
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
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
        | TirExprKind::FieldAccess { expr: inner, .. } => {
            lower_wide_int_in_expr(inner, type_table);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::MethodCall { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
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
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Minimum number of cases required for `br_table` optimization
const SWITCH_MIN_CASES: usize = 8;

/// Minimum density (cases / range) for `br_table` to be worthwhile
const SWITCH_DENSITY_THRESHOLD: f64 = 0.75;

/// Maximum range size for `br_table` (to avoid huge jump tables)
const SWITCH_MAX_RANGE: i64 = 1024;

/// Analysis result for converting Match to Switch
struct SwitchAnalysis {
    /// Minimum value in the switch range
    min_value: i64,
    /// Maximum value in the switch range
    max_value: i64,
    /// Map from value to arm index in the original arms
    value_to_arm: Vec<(i64, usize)>,
    /// Index of the default arm (wildcard/binding), if any
    default_arm: Option<usize>,
}

/// Analyze if a Match expression can be converted to a Switch (for `br_table` optimization)
fn analyze_match_for_switch(
    scrutinee_type: &ResolvedType,
    arms: &[crate::tir::TirMatchArm],
) -> Option<SwitchAnalysis> {
    use crate::tir::PrimitiveType;

    // Only applicable to integer types and enums (enums are i32 discriminants)
    match scrutinee_type {
        ResolvedType::Primitive(
            PrimitiveType::I32
            | PrimitiveType::U32
            | PrimitiveType::I64
            | PrimitiveType::U64
            | PrimitiveType::I16
            | PrimitiveType::U16
            | PrimitiveType::I8
            | PrimitiveType::U8,
        )
        | ResolvedType::Enum { .. } => {}
        _ => return None,
    }

    let mut value_to_arm: Vec<(i64, usize)> = Vec::new();
    let mut default_arm: Option<usize> = None;

    for (arm_idx, arm) in arms.iter().enumerate() {
        // Arms with guards can't use br_table optimization
        if arm.guard.is_some() {
            return None;
        }
        match &arm.pattern {
            TirPattern::Literal(TirLiteralPattern::I128(v)) => {
                value_to_arm.push((*v as i64, arm_idx));
            }
            TirPattern::Literal(TirLiteralPattern::U128(v)) => {
                value_to_arm.push((*v as i64, arm_idx));
            }
            TirPattern::Enum { case_index, .. } => {
                value_to_arm.push((i64::from(*case_index), arm_idx));
            }
            TirPattern::Wildcard | TirPattern::Binding { .. } => {
                // Wildcard/binding is the default case
                if default_arm.is_some() {
                    // Multiple defaults - shouldn't happen, but bail out
                    return None;
                }
                default_arm = Some(arm_idx);
            }
            _ => {
                // Non-integer pattern, can't use br_table
                return None;
            }
        }
    }

    // Need at least MIN_CASES integer literals
    if value_to_arm.len() < SWITCH_MIN_CASES {
        return None;
    }

    // Calculate range
    let min_value = value_to_arm.iter().map(|(v, _)| *v).min().unwrap();
    let max_value = value_to_arm.iter().map(|(v, _)| *v).max().unwrap();
    let range = max_value - min_value + 1;

    // Check range isn't too large
    if range > SWITCH_MAX_RANGE {
        return None;
    }

    // Check density threshold
    #[allow(clippy::cast_precision_loss)]
    let density = value_to_arm.len() as f64 / range as f64;
    if density < SWITCH_DENSITY_THRESHOLD {
        return None;
    }

    Some(SwitchAnalysis {
        min_value,
        max_value,
        value_to_arm,
        default_arm,
    })
}

/// Convert a Match to a Switch expression using the analysis
#[allow(clippy::cast_sign_loss)]
fn match_to_switch(
    scrutinee: Box<TirExpr>,
    arms: &[crate::tir::TirMatchArm],
    analysis: SwitchAnalysis,
    result_type_id: TypeId,
    span: Span,
) -> TirExpr {
    let range = (analysis.max_value - analysis.min_value + 1) as usize;

    // Build a map from offset to arm index (for values not in the map, use default)
    let mut offset_to_arm: Vec<Option<usize>> = vec![None; range];
    for (value, arm_idx) in &analysis.value_to_arm {
        let offset = (*value - analysis.min_value) as usize;
        offset_to_arm[offset] = Some(*arm_idx);
    }

    // Build the arms vector - one block per value in the range
    // Each arm's body is copied from the original match arm
    let switch_arms: Vec<TirBlock> = offset_to_arm
        .iter()
        .map(|maybe_arm_idx| {
            let arm_idx = maybe_arm_idx.unwrap_or_else(|| {
                // Use default arm if available, otherwise the first arm (unreachable case)
                analysis.default_arm.unwrap_or(0)
            });
            let arm = &arms[arm_idx];
            // Wrap the arm body in a block
            TirBlock {
                stmts: vec![TirStmt::new(
                    TirStmtKind::Expr(arm.body.clone()),
                    arm.body.span,
                )],
                span: arm.body.span,
            }
        })
        .collect();

    // Default block - used for values outside the range
    let default_block = if let Some(default_idx) = analysis.default_arm {
        let arm = &arms[default_idx];
        TirBlock {
            stmts: vec![TirStmt::new(
                TirStmtKind::Expr(arm.body.clone()),
                arm.body.span,
            )],
            span: arm.body.span,
        }
    } else {
        // No default - generate unreachable (panic)
        TirBlock {
            stmts: vec![TirStmt::new(
                TirStmtKind::Expr(TirExpr::new(
                    TirExprKind::Call {
                        func: FunctionRef::External {
                            module_source: ModuleSource::internal(),
                            name: "unreachable".to_string(),
                            monomorph_info: None,
                            method_info: None,
                        },
                        args: vec![],
                        type_args: vec![],
                    },
                    TypeTable::NEVER,
                    span,
                )),
                span,
            )],
            span,
        }
    };

    TirExpr::new(
        TirExprKind::Switch {
            scrutinee,
            min_value: analysis.min_value,
            arms: switch_arms,
            default: default_block,
        },
        result_type_id,
        span,
    )
}

/// Lower patterns in a module
///
/// This transforms:
/// - `LetPattern` -> Let statements (allocates temp locals for tuple destructuring)
/// - `IfPattern` -> Let + If statements (allocates scrutinee temp locals)
///
/// By lowering these patterns here, codegen doesn't need preallocate passes.
fn lower_patterns(
    module: &mut TirModule,
    global_variant_map: &IndexMap<String, Vec<(String, u32)>>,
) {
    // Build a map from variant name to case info for quick lookup
    // Start with global variants from all modules (for cross-module pattern matching)
    let mut variant_case_map: IndexMap<String, Vec<(String, u32)>> = global_variant_map.clone();

    // Add module-defined variants (overrides globals for same-module variants)
    for variant in &module.variants {
        let cases: Vec<(String, u32)> = variant
            .cases
            .iter()
            .map(|c| (c.name.clone(), c.index))
            .collect();
        variant_case_map.insert(variant.name.clone(), cases);
    }

    // Build struct fields map from module structs
    let mut struct_fields_map: IndexMap<(String, ModuleSource), Vec<TirField>> = IndexMap::new();
    for s in &module.structs {
        struct_fields_map.insert(
            (s.name.clone(), module.module_source.clone()),
            s.fields.clone(),
        );
    }

    let type_table = module.type_table.borrow();
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(mut body) = func.body.take() {
            // Take ownership of the values to avoid borrow conflicts
            let local_count = func.local_count;
            let local_types = std::mem::take(&mut func.local_types);

            let mut lowerer = PatternLowerer::new(
                local_count,
                local_types,
                &variant_case_map,
                &struct_fields_map,
            );
            lowerer.lower_block(&mut body, &type_table);

            // Put the values back
            let (new_count, new_types) = lowerer.into_parts();
            func.local_count = new_count;
            func.local_types = new_types;
            func.body = Some(body);
        }
    }
}

/// Pattern lowering context - tracks local allocation for a function
struct PatternLowerer<'a> {
    local_count: u32,
    local_types: Vec<TypeId>,
    temp_counter: u32,
    /// Map from variant name to list of (`case_name`, `case_index`) pairs
    variant_case_map: &'a IndexMap<String, Vec<(String, u32)>>,
    /// Map from (`struct_name`, `module_source`) to field definitions
    struct_fields_map: &'a IndexMap<(String, ModuleSource), Vec<TirField>>,
}

impl<'a> PatternLowerer<'a> {
    fn new(
        local_count: u32,
        local_types: Vec<TypeId>,
        variant_case_map: &'a IndexMap<String, Vec<(String, u32)>>,
        struct_fields_map: &'a IndexMap<(String, ModuleSource), Vec<TirField>>,
    ) -> Self {
        Self {
            local_count,
            local_types,
            temp_counter: 0,
            variant_case_map,
            struct_fields_map,
        }
    }

    /// Look up the case index for a variant case by variant name and case name
    fn get_case_index(&self, variant_name: &str, case_name: &str) -> Option<u32> {
        self.variant_case_map
            .get(variant_name)
            .and_then(|cases| cases.iter().find(|(name, _)| name == case_name))
            .map(|(_, index)| *index)
    }

    /// Look up struct field definitions by `type_id`
    fn get_struct_fields(&self, type_id: TypeId, type_table: &TypeTable) -> Option<Vec<TirField>> {
        match type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => self
                .struct_fields_map
                .get(&(name.clone(), module_source.clone()))
                .cloned(),
            _ => None,
        }
    }

    /// Consume the lowerer and return the final local count and types
    fn into_parts(self) -> (u32, Vec<TypeId>) {
        (self.local_count, self.local_types)
    }

    /// Allocate a new local and return its index
    fn alloc_local(&mut self, type_id: TypeId) -> u32 {
        let index = self.local_count;
        self.local_count += 1;
        self.local_types.push(type_id);
        index
    }

    /// Generate a unique temp local name
    fn next_temp_name(&mut self) -> String {
        let name = format!("__pattern_temp_{}", self.temp_counter);
        self.temp_counter += 1;
        name
    }

    /// Check if this is a multi-value builtin call that should not be lowered.
    /// Codegen has a special optimization for these patterns.
    fn is_multivalue_builtin_pattern(
        &self,
        pattern: &TirPattern,
        value: &TirExpr,
        type_table: &TypeTable,
    ) -> bool {
        // Pattern must be a flat tuple with only Binding or Wildcard
        let patterns = match pattern {
            TirPattern::Tuple(patterns) => patterns,
            _ => return false,
        };

        // Verify all sub-patterns are simple bindings or wildcards
        for p in patterns {
            match p {
                TirPattern::Binding { .. } | TirPattern::Wildcard => {}
                _ => return false,
            }
        }

        // Check if value is a builtin call
        let is_builtin_call = match &value.kind {
            TirExprKind::Call { func: func_ref, .. } => {
                if let FunctionRef::External { module_source, .. } = func_ref {
                    module_source.is_core_builtin()
                } else {
                    false
                }
            }
            _ => false,
        };

        if !is_builtin_call {
            return false;
        }

        // Check if return type is a tuple (multi-value return)
        let elem_types = match type_table.get(value.type_id) {
            ResolvedType::Tuple(types) => types,
            _ => return false,
        };

        // Verify pattern length matches tuple element count
        patterns.len() == elem_types.len()
    }

    /// Lower patterns in a block
    fn lower_block(&mut self, block: &mut TirBlock, type_table: &TypeTable) {
        // Process statements, potentially expanding LetPattern into multiple statements
        let mut new_stmts = Vec::with_capacity(block.stmts.len());

        for stmt in std::mem::take(&mut block.stmts) {
            self.lower_stmt(stmt, &mut new_stmts, type_table);
        }

        block.stmts = new_stmts;
    }

    /// Lower a statement, potentially expanding it into multiple statements
    fn lower_stmt(&mut self, stmt: TirStmt, out: &mut Vec<TirStmt>, type_table: &TypeTable) {
        match stmt.kind {
            TirStmtKind::LetPattern {
                pattern,
                is_mut,
                value,
            } => {
                // Don't lower multi-value builtin calls - codegen has a special optimization for them
                if self.is_multivalue_builtin_pattern(&pattern, &value, type_table) {
                    let mut value = value;
                    self.lower_expr(&mut value, type_table);
                    out.push(TirStmt::new(
                        TirStmtKind::LetPattern {
                            pattern,
                            is_mut,
                            value,
                        },
                        stmt.span,
                    ));
                } else {
                    // Lower LetPattern to explicit Let statements
                    self.lower_let_pattern(&pattern, is_mut, value, stmt.span, out, type_table);
                }
            }
            TirStmtKind::IfPattern {
                mut scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                // Lower expressions in scrutinee first
                self.lower_expr(&mut scrutinee, type_table);

                // Check if this is an Option, custom Variant, or Enum pattern that we can lower
                // Match ergonomics: peel Ref/MutRef to find the underlying type
                let mut scrutinee_type_id = scrutinee.type_id;
                while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(scrutinee_type_id)
                {
                    scrutinee_type_id = *inner;
                }
                let scrutinee_type = type_table.get(scrutinee_type_id);
                let can_lower = matches!(
                    scrutinee_type,
                    ResolvedType::Variant { .. }
                        | ResolvedType::GenericInstance { .. }
                        | ResolvedType::Enum { .. }
                );

                if matches!(pattern, TirPattern::Struct { .. }) {
                    // Struct patterns are always irrefutable — lower to let bindings + then block
                    self.lower_if_pattern_struct(
                        scrutinee, &pattern, then_block, stmt.span, out, type_table,
                    );
                } else if can_lower {
                    // Lower Option, Variant, and Enum patterns to Let + If
                    self.lower_if_pattern_option(
                        scrutinee, &pattern, then_block, else_block, stmt.span, out, type_table,
                    );
                } else {
                    // All IfPattern statements should have Option/Variant/Enum scrutinee types
                    // after proper type checking. If we reach here, it's a compiler bug.
                    panic!(
                        "IfPattern with unexpected scrutinee type {scrutinee_type:?} - this should not happen after type checking"
                    );
                }
            }
            TirStmtKind::Let {
                value,
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                ..
            } => {
                // Lower expressions inside the Let value
                let mut value = value;
                self.lower_expr(&mut value, type_table);
                out.push(TirStmt::new(
                    TirStmtKind::Let {
                        name,
                        local_index,
                        is_mut,
                        is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Expr(mut expr) => {
                self.lower_expr(&mut expr, type_table);
                out.push(TirStmt::new(TirStmtKind::Expr(expr), stmt.span));
            }
            TirStmtKind::Return { value } => {
                let value = value.map(|mut v| {
                    self.lower_expr(&mut v, type_table);
                    v
                });
                out.push(TirStmt::new(TirStmtKind::Return { value }, stmt.span));
            }
            TirStmtKind::If {
                condition,
                mut then_block,
                mut else_block,
            } => {
                let mut condition = condition;
                self.lower_expr(&mut condition, type_table);
                self.lower_block(&mut then_block, type_table);
                if let Some(ref mut else_blk) = else_block {
                    self.lower_block(else_blk, type_table);
                }
                out.push(TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Loop { mut body } => {
                self.lower_block(&mut body, type_table);
                out.push(TirStmt::new(TirStmtKind::Loop { body }, stmt.span));
            }
            TirStmtKind::LabeledBlock { label, mut block } => {
                self.lower_block(&mut block, type_table);
                out.push(TirStmt::new(
                    TirStmtKind::LabeledBlock { label, block },
                    stmt.span,
                ));
            }
            TirStmtKind::Break { label, value } => {
                let value = value.map(|mut v| {
                    self.lower_expr(&mut v, type_table);
                    v
                });
                out.push(TirStmt::new(TirStmtKind::Break { label, value }, stmt.span));
            }
            TirStmtKind::Continue => {
                out.push(TirStmt::new(TirStmtKind::Continue, stmt.span));
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    /// Lower `LetPattern` to explicit Let statements
    fn lower_let_pattern(
        &mut self,
        pattern: &TirPattern,
        is_mut: bool,
        value: TirExpr,
        span: Span,
        out: &mut Vec<TirStmt>,
        type_table: &TypeTable,
    ) {
        // First, lower any expressions inside the value
        let mut value = value;
        self.lower_expr(&mut value, type_table);

        match pattern {
            TirPattern::Tuple(sub_patterns) => {
                // Allocate temp local for the tuple
                let tuple_temp_index = self.alloc_local(value.type_id);
                let tuple_temp_name = self.next_temp_name();

                // Create Let for the tuple
                let tuple_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: tuple_temp_name.clone(),
                        local_index: tuple_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(tuple_let);

                // Get element types
                let elem_types = if let ResolvedType::Tuple(types) =
                    type_table.get(type_table.get_local_type(tuple_temp_index, &self.local_types))
                {
                    types.clone()
                } else {
                    vec![TypeTable::UNKNOWN; sub_patterns.len()]
                };

                // Create Let for each element
                for (i, (sub_pattern, elem_type)) in
                    sub_patterns.iter().zip(elem_types.iter()).enumerate()
                {
                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: tuple_temp_index,
                                    name: tuple_temp_name.clone(),
                                },
                                type_table.get_local_type(tuple_temp_index, &self.local_types),
                                span,
                            )),
                            field_index: i as u32,
                            field_name: format!("{i}"),
                        },
                        *elem_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        sub_pattern,
                        is_mut,
                        field_access,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                // Direct binding - just create a Let
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: *local_index,
                        is_mut,
                        is_reactive: false,
                        type_id: *type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(let_stmt);
            }
            TirPattern::Wildcard => {
                // Evaluate value for side effects but discard
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
            TirPattern::Variant {
                bindings,
                payload_type,
                ..
            } => {
                // For variant patterns in LetPattern, extract payload
                // Allocate temp for variant
                let variant_temp_index = self.alloc_local(value.type_id);
                let variant_temp_name = self.next_temp_name();

                let variant_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: variant_temp_name.clone(),
                        local_index: variant_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(variant_let);

                // If there are bindings, extract payload
                if let Some(binding) = bindings.first() {
                    let payload_expr = TirExpr::new(
                        TirExprKind::VariantPayload {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: variant_temp_index,
                                    name: variant_temp_name,
                                },
                                type_table.get_local_type(variant_temp_index, &self.local_types),
                                span,
                            )),
                            case_index: 0, // Will be refined when we have more info
                            payload_type: *payload_type,
                        },
                        *payload_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        binding,
                        is_mut,
                        payload_expr,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Struct { fields, .. } => {
                // Allocate temp local for the struct
                let struct_temp_index = self.alloc_local(value.type_id);
                let struct_temp_name = self.next_temp_name();

                let struct_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: struct_temp_name.clone(),
                        local_index: struct_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(struct_let);

                // Get field type info from struct definition
                let struct_fields_info = self.get_struct_fields(
                    type_table.get_local_type(struct_temp_index, &self.local_types),
                    type_table,
                );

                for field in fields {
                    let field_type = struct_fields_info
                        .as_ref()
                        .and_then(|info| {
                            info.iter()
                                .find(|f| f.name == field.field_name)
                                .map(|f| f.type_id)
                        })
                        .unwrap_or(TypeTable::UNKNOWN);

                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: struct_temp_index,
                                    name: struct_temp_name.clone(),
                                },
                                type_table.get_local_type(struct_temp_index, &self.local_types),
                                span,
                            )),
                            field_index: field.field_index,
                            field_name: field.field_name.clone(),
                        },
                        field_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        &field.pattern,
                        is_mut,
                        field_access,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Literal(_) | TirPattern::Enum { .. } => {
                // Literal/Enum patterns don't bind anything, just evaluate for side effects
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
        }
    }

    /// Helper to lower a pattern to Let statements given an already-evaluated value
    fn lower_pattern_to_lets(
        &mut self,
        pattern: &TirPattern,
        is_mut: bool,
        value: TirExpr,
        span: Span,
        out: &mut Vec<TirStmt>,
        type_table: &TypeTable,
    ) {
        match pattern {
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                // Match ergonomics: if binding type is &T but value type is T,
                // wrap value in a Ref operation to create the reference.
                let value = if matches!(type_table.get(*type_id), ResolvedType::Ref(_))
                    && !matches!(
                        type_table.get(value.type_id),
                        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                    ) {
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(value),
                        },
                        *type_id,
                        span,
                    )
                } else {
                    value
                };
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: *local_index,
                        is_mut,
                        is_reactive: false,
                        type_id: *type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(let_stmt);
            }
            TirPattern::Tuple(sub_patterns) => {
                // Nested tuple - allocate temp and recurse
                let tuple_temp_index = self.alloc_local(value.type_id);
                let tuple_temp_name = self.next_temp_name();

                let tuple_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: tuple_temp_name.clone(),
                        local_index: tuple_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(tuple_let);

                let elem_types = if let ResolvedType::Tuple(types) =
                    type_table.get(type_table.get_local_type(tuple_temp_index, &self.local_types))
                {
                    types.clone()
                } else {
                    vec![TypeTable::UNKNOWN; sub_patterns.len()]
                };

                for (i, (sub_pattern, elem_type)) in
                    sub_patterns.iter().zip(elem_types.iter()).enumerate()
                {
                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: tuple_temp_index,
                                    name: tuple_temp_name.clone(),
                                },
                                type_table.get_local_type(tuple_temp_index, &self.local_types),
                                span,
                            )),
                            field_index: i as u32,
                            field_name: format!("{i}"),
                        },
                        *elem_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        sub_pattern,
                        is_mut,
                        field_access,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Wildcard => {
                // Discard value
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
            TirPattern::Variant {
                bindings,
                payload_type,
                ..
            } => {
                if let Some(binding) = bindings.first() {
                    // Allocate temp and extract payload
                    let variant_temp_index = self.alloc_local(value.type_id);
                    let variant_temp_name = self.next_temp_name();

                    let variant_let = TirStmt::new(
                        TirStmtKind::Let {
                            name: variant_temp_name.clone(),
                            local_index: variant_temp_index,
                            is_mut: false,
                            is_reactive: false,
                            type_id: value.type_id,
                            value,
                            skip_value_copy: false,
                        },
                        span,
                    );
                    out.push(variant_let);

                    let payload_expr = TirExpr::new(
                        TirExprKind::VariantPayload {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: variant_temp_index,
                                    name: variant_temp_name,
                                },
                                type_table.get_local_type(variant_temp_index, &self.local_types),
                                span,
                            )),
                            case_index: 0,
                            payload_type: *payload_type,
                        },
                        *payload_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        binding,
                        is_mut,
                        payload_expr,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Struct { fields, .. } => {
                // Nested struct - allocate temp and recurse
                let struct_temp_index = self.alloc_local(value.type_id);
                let struct_temp_name = self.next_temp_name();

                let struct_let = TirStmt::new(
                    TirStmtKind::Let {
                        name: struct_temp_name.clone(),
                        local_index: struct_temp_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: value.type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                out.push(struct_let);

                let struct_fields_info = self.get_struct_fields(
                    type_table.get_local_type(struct_temp_index, &self.local_types),
                    type_table,
                );

                for field in fields {
                    let field_type = struct_fields_info
                        .as_ref()
                        .and_then(|info| {
                            info.iter()
                                .find(|f| f.name == field.field_name)
                                .map(|f| f.type_id)
                        })
                        .unwrap_or(TypeTable::UNKNOWN);

                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: struct_temp_index,
                                    name: struct_temp_name.clone(),
                                },
                                type_table.get_local_type(struct_temp_index, &self.local_types),
                                span,
                            )),
                            field_index: field.field_index,
                            field_name: field.field_name.clone(),
                        },
                        field_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        &field.pattern,
                        is_mut,
                        field_access,
                        span,
                        out,
                        type_table,
                    );
                }
            }
            TirPattern::Literal(_) | TirPattern::Enum { .. } => {
                // Just evaluate for side effects (no bindings)
                out.push(TirStmt::new(TirStmtKind::Expr(value), span));
            }
        }
    }

    /// Insert deref expressions to peel `Ref`/`MutRef` from a scrutinee expression.
    fn peel_ref_scrutinee(&self, mut scrutinee: TirExpr, type_table: &TypeTable) -> TirExpr {
        loop {
            match type_table.get(scrutinee.type_id) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    let inner = *inner;
                    let span = scrutinee.span;
                    scrutinee = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(scrutinee),
                        },
                        inner,
                        span,
                    );
                }
                _ => return scrutinee,
            }
        }
    }

    /// Lower a struct `IfPattern` to let bindings + then block (unconditional).
    ///
    /// Struct patterns are always irrefutable, so the else branch is discarded.
    #[allow(clippy::too_many_arguments)]
    fn lower_if_pattern_struct(
        &mut self,
        scrutinee: TirExpr,
        pattern: &TirPattern,
        mut then_block: TirBlock,
        span: Span,
        out: &mut Vec<TirStmt>,
        type_table: &TypeTable,
    ) {
        // Match ergonomics: dereference Ref/MutRef scrutinee
        let scrutinee = self.peel_ref_scrutinee(scrutinee, type_table);

        if let TirPattern::Struct { fields, .. } = pattern {
            // Lower the scrutinee into a temp
            let struct_temp_index = self.alloc_local(scrutinee.type_id);
            let struct_temp_name = self.next_temp_name();

            out.push(TirStmt::new(
                TirStmtKind::Let {
                    name: struct_temp_name.clone(),
                    local_index: struct_temp_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: scrutinee.type_id,
                    value: scrutinee.clone(),
                    skip_value_copy: false,
                },
                span,
            ));

            let struct_fields_info = self.get_struct_fields(scrutinee.type_id, type_table);

            for field in fields {
                let field_type = struct_fields_info
                    .as_ref()
                    .and_then(|info| {
                        info.iter()
                            .find(|f| f.name == field.field_name)
                            .map(|f| f.type_id)
                    })
                    .unwrap_or(TypeTable::UNKNOWN);

                let field_access = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
                                index: struct_temp_index,
                                name: struct_temp_name.clone(),
                            },
                            scrutinee.type_id,
                            span,
                        )),
                        field_index: field.field_index,
                        field_name: field.field_name.clone(),
                    },
                    field_type,
                    span,
                );

                self.lower_pattern_to_lets(
                    &field.pattern,
                    false,
                    field_access,
                    span,
                    out,
                    type_table,
                );
            }

            // Emit then block unconditionally (struct patterns are irrefutable)
            self.lower_block(&mut then_block, type_table);
            for s in then_block.stmts {
                out.push(s);
            }
        }
    }

    /// Lower an Option `IfPattern` to Let + If
    ///
    /// Transforms:
    ///   `if let Some(x) = opt { then } else { else }`
    /// To:
    ///   `let $temp = opt;
    ///    if VariantTest($temp, case) { let x = VariantPayload($temp); then } else { else }`
    #[allow(clippy::too_many_arguments)]
    fn lower_if_pattern_option(
        &mut self,
        scrutinee: TirExpr,
        pattern: &TirPattern,
        mut then_block: TirBlock,
        mut else_block: Option<TirBlock>,
        span: Span,
        out: &mut Vec<TirStmt>,
        type_table: &TypeTable,
    ) {
        // Match ergonomics: dereference Ref/MutRef scrutinee
        let scrutinee = self.peel_ref_scrutinee(scrutinee, type_table);

        // Allocate a temp local for the scrutinee to avoid re-evaluation
        let scrutinee_temp_index = self.alloc_local(scrutinee.type_id);
        let scrutinee_temp_name = self.next_temp_name();

        // Create Let for the scrutinee temp
        let scrutinee_let = TirStmt::new(
            TirStmtKind::Let {
                name: scrutinee_temp_name.clone(),
                local_index: scrutinee_temp_index,
                is_mut: false,
                is_reactive: false,
                type_id: scrutinee.type_id,
                value: scrutinee.clone(),
                skip_value_copy: false,
            },
            span,
        );
        out.push(scrutinee_let);

        // Create a reference to the temp local
        let temp_ref = TirExpr::new(
            TirExprKind::Local {
                index: scrutinee_temp_index,
                name: scrutinee_temp_name,
            },
            scrutinee.type_id,
            span,
        );

        // Generate condition and binding statements
        let (condition, mut binding_stmts) =
            self.pattern_to_condition_and_bindings(pattern, temp_ref, span, type_table);

        // Lower the binding statements (they may contain nested patterns)
        let mut lowered_bindings = Vec::new();
        for stmt in binding_stmts.drain(..) {
            self.lower_stmt(stmt, &mut lowered_bindings, type_table);
        }

        // Prepend binding statements to the then block
        let mut new_then_stmts = lowered_bindings;
        // Lower the then block
        self.lower_block(&mut then_block, type_table);
        new_then_stmts.extend(then_block.stmts);
        then_block.stmts = new_then_stmts;

        // Lower the else block if present
        if let Some(ref mut else_blk) = else_block {
            self.lower_block(else_blk, type_table);
        }

        // Create a regular If statement
        out.push(TirStmt::new(
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            },
            span,
        ));
    }

    /// Convert a pattern to a condition expression and binding statements
    fn pattern_to_condition_and_bindings(
        &mut self,
        pattern: &TirPattern,
        scrutinee: TirExpr,
        span: Span,
        type_table: &TypeTable,
    ) -> (TirExpr, Vec<TirStmt>) {
        let mut binding_stmts = Vec::new();

        match pattern {
            TirPattern::Variant {
                variant_name,
                bindings,
                payload_type,
                ..
            } => {
                // All variants (including Option) use VariantTest/VariantPayload

                // Get variant type name
                let scrutinee_type = type_table.get(scrutinee.type_id);
                let variant_type_name = match scrutinee_type {
                    ResolvedType::Variant { name, .. }
                    | ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                    _ => None,
                };

                let condition = if let Some(ref vt_name) = variant_type_name {
                    let case_index =
                        self.get_case_index(vt_name, variant_name)
                            .unwrap_or_else(|| {
                                panic!("Unknown case {variant_name} for variant {vt_name}")
                            });
                    TirExpr::new(
                        TirExprKind::VariantTest {
                            expr: Box::new(scrutinee.clone()),
                            case_index,
                            case_name: variant_name.clone(),
                        },
                        TypeTable::BOOL,
                        span,
                    )
                } else {
                    // Fallback for unknown types - should not happen after monomorphization
                    TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span)
                };

                // Generate binding statements for the payload
                if let Some(binding) = bindings.first() {
                    let case_index = variant_type_name
                        .as_ref()
                        .and_then(|vt| self.get_case_index(vt, variant_name))
                        .unwrap_or(0);

                    let payload_expr = TirExpr::new(
                        TirExprKind::VariantPayload {
                            expr: Box::new(scrutinee),
                            case_index,
                            payload_type: *payload_type,
                        },
                        *payload_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        binding,
                        false,
                        payload_expr,
                        span,
                        &mut binding_stmts,
                        type_table,
                    );
                }

                (condition, binding_stmts)
            }
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                // Match ergonomics: if binding type is &T but value type is T,
                // wrap value in a Ref operation.
                let value = if matches!(type_table.get(*type_id), ResolvedType::Ref(_))
                    && !matches!(
                        type_table.get(scrutinee.type_id),
                        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                    ) {
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(scrutinee),
                        },
                        *type_id,
                        span,
                    )
                } else {
                    scrutinee
                };
                // Always matches, bind the value
                let let_stmt = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: *local_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: *type_id,
                        value,
                        skip_value_copy: false,
                    },
                    span,
                );
                binding_stmts.push(let_stmt);

                (
                    TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span),
                    binding_stmts,
                )
            }
            TirPattern::Wildcard => (
                TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span),
                binding_stmts,
            ),
            TirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => {
                // Enum pattern: compare i32 discriminant using EnumConstruct
                let condition = TirExpr::new(
                    TirExprKind::Binary {
                        left: Box::new(scrutinee),
                        op: TirBinaryOp::Eq,
                        right: Box::new(TirExpr::new(
                            TirExprKind::EnumConstruct {
                                enum_type: *enum_type,
                                case_index: *case_index,
                                case_name: case_name.clone(),
                            },
                            *enum_type,
                            span,
                        )),
                    },
                    TypeTable::BOOL,
                    span,
                );
                // No bindings for enum patterns (no payload)
                (condition, binding_stmts)
            }
            TirPattern::Struct { fields, .. } => {
                // Struct patterns are always irrefutable — always match
                // Extract fields into bindings
                let struct_fields_info = self.get_struct_fields(scrutinee.type_id, type_table);

                for field in fields {
                    let field_type = struct_fields_info
                        .as_ref()
                        .and_then(|info| {
                            info.iter()
                                .find(|f| f.name == field.field_name)
                                .map(|f| f.type_id)
                        })
                        .unwrap_or(TypeTable::UNKNOWN);

                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(scrutinee.clone()),
                            field_index: field.field_index,
                            field_name: field.field_name.clone(),
                        },
                        field_type,
                        span,
                    );

                    self.lower_pattern_to_lets(
                        &field.pattern,
                        false,
                        field_access,
                        span,
                        &mut binding_stmts,
                        type_table,
                    );
                }

                (
                    TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span),
                    binding_stmts,
                )
            }
            TirPattern::Tuple(_) | TirPattern::Literal(_) => {
                // These shouldn't appear at the top level of IfPattern
                // Just return true for now
                (
                    TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span),
                    binding_stmts,
                )
            }
        }
    }

    /// Lower expressions (recurse into sub-expressions)
    fn lower_expr(&mut self, expr: &mut TirExpr, type_table: &TypeTable) {
        match &mut expr.kind {
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.lower_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.lower_expr(condition, type_table);
                self.lower_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.lower_block(else_blk, type_table);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                // First, recursively lower sub-expressions
                self.lower_expr(scrutinee, type_table);
                for arm in arms.iter_mut() {
                    if let Some(guard) = &mut arm.guard {
                        self.lower_expr(guard, type_table);
                    }
                    self.lower_expr(&mut arm.body, type_table);
                }

                // Match ergonomics: insert deref if scrutinee is Ref/MutRef
                while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(scrutinee.type_id)
                {
                    let inner = *inner;
                    let span = scrutinee.span;
                    let old = std::mem::replace(
                        scrutinee.as_mut(),
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                    );
                    *scrutinee.as_mut() = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(old),
                        },
                        inner,
                        span,
                    );
                }

                // Analyze if this Match can be converted to Switch (for br_table)
                let scrutinee_type = type_table.get(scrutinee.type_id).clone();
                if let Some(analysis) = analyze_match_for_switch(&scrutinee_type, arms) {
                    // Convert to Switch
                    let result_type_id = expr.type_id;
                    let span = expr.span;

                    // Take ownership of scrutinee and arms to build Switch
                    let scrutinee_owned = std::mem::replace(
                        scrutinee,
                        Box::new(TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span)),
                    );
                    let arms_owned = std::mem::take(arms);

                    let switch_expr = match_to_switch(
                        scrutinee_owned,
                        &arms_owned,
                        analysis,
                        result_type_id,
                        span,
                    );
                    expr.kind = switch_expr.kind;
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.lower_expr(left, type_table);
                self.lower_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                self.lower_expr(inner, type_table);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::MethodCall { args, .. }
            | TirExprKind::StaticCall { args, .. }
            | TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.lower_expr(arg, type_table);
                }
            }
            TirExprKind::Index { expr: arr, index }
            | TirExprKind::Assign {
                target: arr,
                value: index,
            } => {
                self.lower_expr(arr, type_table);
                self.lower_expr(index, type_table);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.lower_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.lower_expr(elem, type_table);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.lower_expr(callee, type_table);
                for arg in args {
                    self.lower_expr(arg, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.lower_expr(body, type_table);
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.lower_expr(functor, type_table);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.lower_expr(p, type_table);
                }
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.lower_expr(value, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.lower_expr(scrutinee, type_table);
                for arm in arms {
                    self.lower_block(arm, type_table);
                }
                self.lower_block(default, type_table);
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }
    }
}

/// Helper trait extension for `TypeTable` to get local type
trait TypeTableExt {
    fn get_local_type(&self, index: u32, local_types: &[TypeId]) -> TypeId;
}

impl TypeTableExt for TypeTable {
    fn get_local_type(&self, index: u32, local_types: &[TypeId]) -> TypeId {
        local_types
            .get(index as usize)
            .copied()
            .unwrap_or(TypeTable::UNKNOWN)
    }
}

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
    let mut lazy_inits: Vec<(usize, String, ModuleSource, TypeId, TirExpr, Vec<TypeId>)> =
        Vec::new();

    for (idx, global) in module.globals.iter_mut().enumerate() {
        if !is_constant_initializer(&global.initializer) {
            // Save the original initializer with index and local types
            lazy_inits.push((
                idx,
                global.name.clone(),
                global.module_source.clone(),
                global.ty,
                global.initializer.clone(),
                global.local_types.clone(),
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
    let mut merged_local_types: Vec<TypeId> = Vec::new();

    for (_, name, module_source, _, mut initializer, local_types) in sorted_inits {
        // Renumber locals if this isn't the first global (to avoid index conflicts)
        let offset = u32::try_from(merged_local_types.len()).unwrap();
        if offset > 0 && !local_types.is_empty() {
            renumber_locals_in_expr(&mut initializer, offset);
        }
        merged_local_types.extend_from_slice(&local_types);

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

    let local_count = u32::try_from(merged_local_types.len()).unwrap();

    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    let init_func = TirFunction {
        is_async: false,
        name: "__initialize_module".to_string(),
        is_pub: true, // pub so it can be called from entry module's __initialize_modules
        is_export: false, // Internal function, not a world export
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: Vec::new(),
        return_type: TypeTable::UNIT,
        effects: Vec::new(),
        body: Some(init_body),
        span,
        local_count,
        local_types: merged_local_types,
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
    };

    module.functions.push(Rc::new(RefCell::new(init_func)));
}

/// Collect global variable references from an expression
fn collect_global_refs(expr: &TirExpr, refs: &mut IndexSet<String>) {
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
    lazy_inits: &[(usize, String, ModuleSource, TypeId, TirExpr, Vec<TypeId>)],
    _all_globals: &[TirGlobal],
) -> Vec<(usize, String, ModuleSource, TypeId, TirExpr, Vec<TypeId>)> {
    if lazy_inits.len() <= 1 {
        return lazy_inits.to_vec();
    }

    // Build a map from global name to its index in lazy_inits
    let name_to_idx: IndexMap<String, usize> = lazy_inits
        .iter()
        .enumerate()
        .map(|(i, (_, name, ..))| (name.clone(), i))
        .collect();

    // Build dependency graph: deps[i] = set of indices that i depends on
    let mut deps: Vec<IndexSet<usize>> = vec![IndexSet::new(); lazy_inits.len()];

    for (i, (_, _, _, _, initializer, _)) in lazy_inits.iter().enumerate() {
        let mut refs = IndexSet::new();
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
    let mut in_degree: Vec<usize> = deps.iter().map(IndexSet::len).collect();
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
            .map(|(_, (_, name, ..))| name.as_str())
            .collect();
        panic!(
            "Circular dependency detected among global variables: {}",
            in_cycle.join(", ")
        );
    }

    sorted
}

/// Renumber all local variable indices in a TIR expression by adding an offset.
/// Used when merging multiple global initializers into a single `__initialize_module` function.
fn renumber_locals_in_expr(expr: &mut TirExpr, offset: u32) {
    match &mut expr.kind {
        TirExprKind::Local { index, .. } => *index += offset,
        TirExprKind::Binary { left, right, .. } => {
            renumber_locals_in_expr(left, offset);
            renumber_locals_in_expr(right, offset);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
            renumber_locals_in_expr(inner, offset);
        }
        TirExprKind::Index { expr: e, index: i } => {
            renumber_locals_in_expr(e, offset);
            renumber_locals_in_expr(i, offset);
        }
        TirExprKind::Assign { target, value } => {
            renumber_locals_in_expr(target, offset);
            renumber_locals_in_expr(value, offset);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                renumber_locals_in_expr(arg, offset);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. }
        | TirExprKind::IndirectCall {
            callee: receiver,
            args,
        } => {
            renumber_locals_in_expr(receiver, offset);
            for arg in args {
                renumber_locals_in_expr(arg, offset);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                renumber_locals_in_expr(p, offset);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                renumber_locals_in_expr(&mut field.value, offset);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                renumber_locals_in_expr(elem, offset);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            renumber_locals_in_block(block, offset);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            renumber_locals_in_expr(condition, offset);
            renumber_locals_in_block(then_branch, offset);
            if let Some(eb) = else_branch {
                renumber_locals_in_block(eb, offset);
            }
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            renumber_locals_in_expr(scrutinee, offset);
            for arm in arms {
                renumber_locals_in_pattern(&mut arm.pattern, offset);
                if let Some(ref mut guard) = arm.guard {
                    renumber_locals_in_expr(guard, offset);
                }
                renumber_locals_in_expr(&mut arm.body, offset);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            renumber_locals_in_expr(value, offset);
        }
        // Leaf nodes with no locals
        _ => {}
    }
}

fn renumber_locals_in_block(block: &mut TirBlock, offset: u32) {
    for stmt in &mut block.stmts {
        renumber_locals_in_stmt(stmt, offset);
    }
}

fn renumber_locals_in_stmt(stmt: &mut TirStmt, offset: u32) {
    match &mut stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            *local_index += offset;
            renumber_locals_in_expr(value, offset);
        }
        TirStmtKind::Expr(expr) => renumber_locals_in_expr(expr, offset),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                renumber_locals_in_expr(v, offset);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            renumber_locals_in_expr(condition, offset);
            renumber_locals_in_block(then_block, offset);
            if let Some(eb) = else_block {
                renumber_locals_in_block(eb, offset);
            }
        }
        TirStmtKind::Loop { body } => renumber_locals_in_block(body, offset),
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                renumber_locals_in_expr(v, offset);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LabeledBlock { block, .. } => renumber_locals_in_block(block, offset),
        TirStmtKind::IfPattern {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => {
            renumber_locals_in_expr(scrutinee, offset);
            renumber_locals_in_pattern(pattern, offset);
            renumber_locals_in_block(then_block, offset);
            if let Some(eb) = else_block {
                renumber_locals_in_block(eb, offset);
            }
        }
        TirStmtKind::LetPattern { pattern, value, .. } => {
            renumber_locals_in_pattern(pattern, offset);
            renumber_locals_in_expr(value, offset);
        }
        TirStmtKind::TaskReturn { .. } => {}
    }
}

fn renumber_locals_in_pattern(pattern: &mut TirPattern, offset: u32) {
    match pattern {
        TirPattern::Binding { local_index, .. } => *local_index += offset,
        TirPattern::Tuple(patterns) => {
            for p in patterns {
                renumber_locals_in_pattern(p, offset);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                renumber_locals_in_pattern(p, offset);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                renumber_locals_in_pattern(&mut f.pattern, offset);
            }
        }
        TirPattern::Wildcard | TirPattern::Literal(_) | TirPattern::Enum { .. } => {}
    }
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
    modules_with_init.sort_by_key(|ms| i32::from(ms == &entry_source));

    let span = Span::new(0, 0, 1, 1);

    // Get mutable reference to entry module
    let entry_module = modules.get_mut(&entry_source).unwrap();

    // Create __modules_initialized flag global
    let init_flag_global = TirGlobal {
        name: "__modules_initialized".to_string(),
        ty: TypeTable::BOOL,
        initializer: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, span),
        mutable: true,
        wado_mutable: true,
        is_pub: false,
        module_source: entry_source.clone(),
        span,
        is_nullable: false,
        local_types: Vec::new(),
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
        is_async: false,
        name: "__initialize_modules".to_string(),
        is_pub: false,    // Not pub - internal to entry module
        is_export: false, // Internal function, not a world export
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
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
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

/// Lowers primitive boxing to explicit `Box<T>` struct operations.
///
/// Before this pass, codegen was responsible for:
/// - Detecting address-taken primitive locals and allocating box structs
/// - Boxing/unboxing `&primitive` and `*ref_to_primitive`
/// - Boxing primitives in `Option<primitive>` and `Option::Some(primitive)`
///
/// After this pass, all boxing is expressed as normal struct operations on
/// `Box<T>` types (defined in `core/internal.wado`), and codegen needs no
/// special boxing knowledge.
struct BoxLowerer {
    /// Mapping from inner `TypeId` to Box<T> struct type ID.
    /// e.g., `TypeTable::I32` → `TypeId` for Struct("Box<i32>")
    box_struct_types: IndexMap<TypeId, TypeId>,
    /// Set of all Box<T> struct type IDs (for fast lookup).
    box_type_ids: IndexSet<TypeId>,
    /// Generated Box<T> struct definitions to add to the module.
    generated_structs: Vec<TirStruct>,
    /// Module source for registering Box types in the type table.
    /// Box is conceptually part of core:internal, matching the module where Box
    /// is identified in downstream phases (`wir_build` checks `is_core_internal()`).
    box_module_source: ModuleSource,
    /// Struct fields indexed by (name, `module_source`) for deref assign expansion.
    struct_fields_map: IndexMap<(String, ModuleSource), Vec<TirField>>,
    /// Variant names from all modules, used to identify `GenericInstance` types
    /// that are variants and need boxing.
    variant_names: IndexSet<String>,
}

impl BoxLowerer {
    fn new() -> Self {
        Self {
            box_struct_types: IndexMap::new(),
            box_type_ids: IndexSet::new(),
            generated_structs: Vec::new(),
            box_module_source: ModuleSource::Core {
                name: "internal".to_string(),
            },
            struct_fields_map: IndexMap::new(),
            variant_names: IndexSet::<String>::new(),
        }
    }

    /// Get or create a Box<T> struct type for the given inner type.
    fn get_or_create_box_type(
        &mut self,
        inner_type_id: TypeId,
        type_table: &mut TypeTable,
    ) -> TypeId {
        if let Some(&box_type) = self.box_struct_types.get(&inner_type_id) {
            return box_type;
        }

        // Create the Box struct type name: e.g., "Box<i32>"
        let inner_name = type_table.mangle_type_name(inner_type_id);
        let struct_name = mangle_generic_name("Box", &[inner_name]);

        // Register under core:internal — Box is a compiler-synthesized type that
        // downstream phases identify via `is_core_internal()`.
        let struct_type_id = type_table.make_monomorphized_struct(
            struct_name.clone(),
            self.box_module_source.clone(),
            "Box".to_string(),
        );

        // Create the TirStruct definition with a single `value` field
        let tir_struct = TirStruct {
            name: struct_name,
            is_pub: true,
            type_params: Vec::new(),
            monomorph_info: Some(MonomorphInfo {
                generic_name: "Box".to_string(),
                type_args: vec![inner_type_id],
                is_blanket: false,
            }),
            fields: vec![TirField {
                name: "value".to_string(),
                is_pub: false,
                type_id: inner_type_id,
                index: 0,
                span: Span::new(0, 0, 0, 0),
                is_hidden: false,
                serde_rename: None,
                serde_default: false,
            }],
            span: Span::new(0, 0, 0, 0),
            serde_rename_all: None,
        };

        self.generated_structs.push(tir_struct);
        self.box_struct_types.insert(inner_type_id, struct_type_id);
        self.box_type_ids.insert(struct_type_id);

        struct_type_id
    }

    /// Get the inner (value) `TypeId` for a Box struct type, if it is one.
    fn get_box_inner_type(&self, type_id: TypeId) -> Option<TypeId> {
        for (&inner, &box_type) in &self.box_struct_types {
            if box_type == type_id {
                return Some(inner);
            }
        }
        None
    }

    /// Check if a type is a variant (either directly or as a `GenericInstance` of a variant).
    fn is_variant_type(&self, type_id: TypeId, type_table: &TypeTable) -> bool {
        match type_table.get(type_id) {
            ResolvedType::Variant { .. } => true,
            ResolvedType::GenericInstance { name, .. } => {
                self.variant_names.contains(name.as_str())
            }
            _ => false,
        }
    }

    /// Look up struct fields for a given `TypeId` via the type table.
    fn get_struct_fields(&self, type_id: TypeId, type_table: &TypeTable) -> Option<Vec<TirField>> {
        match type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => self
                .struct_fields_map
                .get(&(name.clone(), module_source.clone()))
                .cloned(),
            _ => None,
        }
    }

    /// Expand `*ref = value` for non-box struct types into field-by-field assignments.
    ///
    /// After `transform_block`, any remaining `Assign { target: Deref(..) }` nodes
    /// are for non-primitive types (structs, String). This pass expands them into:
    ///   let __`deref_ref_N` = `ref_expr`;
    ///   let __`deref_val_N` = `value_expr`;
    ///   __`deref_ref_N.field0` = __`deref_val_N.field0`;
    ///   __`deref_ref_N.field1` = __`deref_val_N.field1`;
    ///   ...
    fn expand_deref_assigns_in_block(
        &self,
        block: &mut TirBlock,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
        type_table: &TypeTable,
    ) {
        let mut new_stmts: Vec<TirStmt> = Vec::with_capacity(block.stmts.len());

        for stmt in std::mem::take(&mut block.stmts) {
            // Recurse into nested blocks first
            new_stmts.push(stmt);
            let stmt = new_stmts.last_mut().unwrap();
            self.expand_deref_assigns_in_stmt(stmt, local_count, local_types, type_table);

            // Check if this stmt is Expr(Assign { target: Deref(..), value })
            let should_expand = matches!(
                &stmt.kind,
                TirStmtKind::Expr(expr) if matches!(
                    &expr.kind,
                    TirExprKind::Assign { target, .. }
                    if matches!(&target.kind, TirExprKind::Unary { op: TirUnaryOp::Deref, .. })
                )
            );

            if !should_expand {
                continue;
            }

            // Extract the assign components and save type info before moves
            let TirStmtKind::Expr(expr) = &mut stmt.kind else {
                continue;
            };
            let TirExprKind::Assign { target, value } = &mut expr.kind else {
                continue;
            };
            let TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: ref_expr,
            } = &mut target.kind
            else {
                continue;
            };

            // Determine the inner struct type from the ref type
            let inner_type_id = match type_table.get(ref_expr.type_id) {
                ResolvedType::MutRef(inner) => *inner,
                // Ref should have been caught by the immutable check
                _ => continue,
            };

            // Look up struct fields
            let Some(fields) = self.get_struct_fields(inner_type_id, type_table) else {
                continue;
            };

            if fields.is_empty() {
                continue;
            }

            // Save type IDs before destructive moves
            let ref_type_id = ref_expr.type_id;
            let span = expr.span;

            // Allocate temp locals
            let ref_local_idx = *local_count;
            *local_count += 1;
            local_types.push(ref_type_id);

            let val_local_idx = *local_count;
            *local_count += 1;
            local_types.push(inner_type_id);

            // Take ownership of ref_expr and value
            let ref_owned = std::mem::replace(
                ref_expr.as_mut(),
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
            );
            let val_owned = std::mem::replace(
                value.as_mut(),
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
            );

            // Remove the original stmt (we just pushed it) and replace with expansion
            new_stmts.pop();

            // let __deref_ref = ref_expr
            new_stmts.push(TirStmt {
                kind: TirStmtKind::Let {
                    local_index: ref_local_idx,
                    name: format!("__deref_ref_{ref_local_idx}"),
                    type_id: ref_type_id,
                    is_mut: false,
                    is_reactive: false,
                    value: ref_owned,
                    skip_value_copy: false,
                },
                span,
            });

            // let __deref_val = value_expr
            new_stmts.push(TirStmt {
                kind: TirStmtKind::Let {
                    local_index: val_local_idx,
                    name: format!("__deref_val_{val_local_idx}"),
                    type_id: inner_type_id,
                    is_mut: false,
                    is_reactive: false,
                    value: val_owned,
                    skip_value_copy: false,
                },
                span,
            });

            // For each field: __deref_ref.field_i = __deref_val.field_i
            for field in &fields {
                let ref_local = TirExpr::new(
                    TirExprKind::Local {
                        index: ref_local_idx,
                        name: format!("__deref_ref_{ref_local_idx}"),
                    },
                    ref_type_id,
                    span,
                );
                let val_local = TirExpr::new(
                    TirExprKind::Local {
                        index: val_local_idx,
                        name: format!("__deref_val_{val_local_idx}"),
                    },
                    inner_type_id,
                    span,
                );

                let assign_target = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(ref_local),
                        field_index: field.index,
                        field_name: field.name.clone(),
                    },
                    field.type_id,
                    span,
                );
                let assign_value = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(val_local),
                        field_index: field.index,
                        field_name: field.name.clone(),
                    },
                    field.type_id,
                    span,
                );

                new_stmts.push(TirStmt {
                    kind: TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Assign {
                            target: Box::new(assign_target),
                            value: Box::new(assign_value),
                        },
                        field.type_id,
                        span,
                    )),
                    span,
                });
            }
        }

        block.stmts = new_stmts;
    }

    /// Recurse into nested blocks within a statement for deref assign expansion.
    fn expand_deref_assigns_in_stmt(
        &self,
        stmt: &mut TirStmt,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
        type_table: &TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.expand_deref_assigns_in_block(
                    then_block,
                    local_count,
                    local_types,
                    type_table,
                );
                if let Some(else_block) = else_block {
                    self.expand_deref_assigns_in_block(
                        else_block,
                        local_count,
                        local_types,
                        type_table,
                    );
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.expand_deref_assigns_in_block(body, local_count, local_types, type_table);
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                self.expand_deref_assigns_in_block(
                    then_block,
                    local_count,
                    local_types,
                    type_table,
                );
                if let Some(else_block) = else_block {
                    self.expand_deref_assigns_in_block(
                        else_block,
                        local_count,
                        local_types,
                        type_table,
                    );
                }
            }
            _ => {}
        }
    }

    /// Transform expressions in a module (called after type table setup).
    ///
    /// This is the per-module phase: transforms function bodies, impl methods,
    /// and global initializers. Also injects generated Box structs into the module.
    fn lower_module_exprs(&mut self, module: &mut TirModule) {
        // Transform expressions in all functions.
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            self.transform_function(&mut func, &module.type_table);
        }

        // Transform impl method bodies
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                self.transform_function(method, &module.type_table);
            }
        }

        // Transform global initializers
        {
            let type_table = module.type_table.borrow();
            for global in &mut module.globals {
                self.transform_expr(&mut global.initializer, &IndexSet::new(), &type_table);
            }
        }

        // Box structs are injected into the core:internal module separately
        // (see lower_modules_indexed)
    }

    /// Scan the type table to find which primitives need Box types.
    fn create_needed_box_types(&mut self, type_table: &mut TypeTable) {
        // Collect base TypeIds that need boxing, plus newtypes.
        // Boxing is required for:
        // - Primitives (except i128/u128 which are already GC types)
        // - Variant types (subtype hierarchy prevents field-by-field deref assignment)
        let mut needs_box_base: IndexSet<TypeId> = IndexSet::new();
        let mut newtype_pairs: Vec<(TypeId, TypeId)> = Vec::new(); // (alias, base)

        for type_id in type_table.iter_type_ids().collect::<Vec<_>>() {
            match type_table.get(type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    let base = type_table.get_ultimate_base_type(inner);
                    let is_prim = matches!(type_table.get(base), ResolvedType::Primitive(p)
                        if !matches!(p, PrimitiveType::I128 | PrimitiveType::U128));
                    let is_variant = self.is_variant_type(base, type_table);
                    let needs_box = is_prim || is_variant;
                    if needs_box {
                        needs_box_base.insert(base);
                        if inner != base {
                            newtype_pairs.push((inner, base));
                        }
                    }
                }
                _ => {}
            }
        }

        // Create Box<T> struct types for each type that needs boxing
        for base_type_id in needs_box_base {
            self.get_or_create_box_type(base_type_id, type_table);
        }

        // Map newtypes to their base type's Box type
        // e.g., Radians (newtype of f64) → Box<f64>
        for (alias_id, base_id) in newtype_pairs {
            if let Some(&box_type_id) = self.box_struct_types.get(&base_id) {
                self.box_struct_types.insert(alias_id, box_type_id);
            }
        }
    }

    /// Rewrite type table entries: Ref(primitive) → Box struct, MutRef(primitive) → Box struct.
    ///
    /// Note: Option(primitive) is NOT rewritten here. The type table keeps `Option(primitive)`
    /// so that codegen and pattern matching can still see the original inner type. The lower
    /// pass transforms variant expressions (`VariantConstruct`) to wrap/unwrap Box structs,
    /// while codegen handles the type mapping from `Option(primitive)` to a nullable Box reference.
    fn rewrite_types(&mut self, type_table: &mut TypeTable) {
        // Collect entries to rewrite (can't mutate while iterating)
        let mut replacements: Vec<(TypeId, ResolvedType)> = Vec::new();

        for type_id in type_table.iter_type_ids().collect::<Vec<_>>() {
            match type_table.get(type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    // Use get_ultimate_base_type to handle newtypes of primitives
                    let base_inner = type_table.get_ultimate_base_type(inner);
                    if let Some(&box_type_id) = self.box_struct_types.get(&base_inner) {
                        // Replace Ref(primitive) with the Box struct type
                        replacements.push((type_id, type_table.get(box_type_id).clone()));
                    }
                }
                _ => {}
            }
        }

        for (type_id, new_type) in &replacements {
            type_table.replace_type(*type_id, new_type.clone());
        }

        // Add all rewritten TypeIds to box_type_ids so that Deref/Assign
        // handlers can recognize them as Box types.
        for (type_id, _) in replacements {
            self.box_type_ids.insert(type_id);
        }
    }

    /// Transform a function's body to use Box<T> struct operations.
    fn transform_function(&self, func: &mut TirFunction, type_table_rc: &Rc<RefCell<TypeTable>>) {
        let type_table = type_table_rc.borrow();

        // Update local_types for address-taken primitive locals
        let address_taken = func.address_taken_locals.clone();
        for &local_idx in &address_taken {
            let local_type_id = func.local_types[local_idx as usize];
            if let Some(&box_type_id) = self.box_struct_types.get(&local_type_id) {
                func.local_types[local_idx as usize] = box_type_id;
            }
        }

        // Transform the function body
        if let Some(body) = &mut func.body {
            self.transform_block(body, &address_taken, &type_table);
        }

        // Expand non-box deref assignments (*ref = value) to field-by-field assignments.
        // After transform_block, any remaining Assign { target: Deref(..) } are for
        // non-primitive struct types. Expand them using the struct fields map.
        if let Some(body) = &mut func.body {
            self.expand_deref_assigns_in_block(
                body,
                &mut func.local_count,
                &mut func.local_types,
                &type_table,
            );
        }

        // Clear address_taken_locals since boxing is now handled in TIR
        func.address_taken_locals.clear();
    }

    /// Transform a block of statements.
    fn transform_block(
        &self,
        block: &mut TirBlock,
        address_taken: &IndexSet<u32>,
        type_table: &TypeTable,
    ) {
        for stmt in &mut block.stmts {
            self.transform_stmt(stmt, address_taken, type_table);
        }
    }

    /// Transform a single statement.
    fn transform_stmt(
        &self,
        stmt: &mut TirStmt,
        address_taken: &IndexSet<u32>,
        type_table: &TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let {
                local_index,
                value,
                type_id,
                ..
            } => {
                // First transform the value expression
                self.transform_expr(value, address_taken, type_table);

                // For address-taken primitive locals, wrap the initial value in Box<T>
                if address_taken.contains(local_index)
                    && let Some(&box_type_id) = self.box_struct_types.get(type_id)
                {
                    let original_value = std::mem::replace(
                        value,
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, stmt.span),
                    );
                    let box_struct_name =
                        if let ResolvedType::Struct { name, .. } = type_table.get(box_type_id) {
                            name.clone()
                        } else {
                            panic!("Box type should be a struct");
                        };
                    *value = TirExpr::new(
                        TirExprKind::StructLiteral {
                            struct_type: box_type_id,
                            struct_name: box_struct_name,
                            fields: vec![TirStructField {
                                name: "value".to_string(),
                                value: original_value,
                                field_index: 0,
                            }],
                        },
                        box_type_id,
                        stmt.span,
                    );
                    // Update the Let's type_id to Box<T>
                    *type_id = box_type_id;
                }
            }
            TirStmtKind::Expr(expr) => {
                self.transform_expr(expr, address_taken, type_table);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                self.transform_expr(expr, address_taken, type_table);
            }
            TirStmtKind::Return { value: None } => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.transform_expr(condition, address_taken, type_table);
                self.transform_block(then_block, address_taken, type_table);
                if let Some(else_block) = else_block {
                    self.transform_block(else_block, address_taken, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.transform_block(body, address_taken, type_table);
            }
            TirStmtKind::Break {
                value: Some(expr), ..
            } => {
                self.transform_expr(expr, address_taken, type_table);
            }
            TirStmtKind::Break { value: None, .. } | TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.transform_block(block, address_taken, type_table);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.transform_expr(scrutinee, address_taken, type_table);
                self.transform_block(then_block, address_taken, type_table);
                if let Some(else_block) = else_block {
                    self.transform_block(else_block, address_taken, type_table);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.transform_expr(value, address_taken, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    /// Transform a single expression.
    ///
    /// This is the core of the boxing lowering. It handles:
    /// 1. `Unary(Ref/MutRef, expr)` for primitives → `StructLiteral(Box<T>)`
    /// 2. `Unary(Ref/MutRef, Local)` for address-taken → just `Local` (Box IS the ref)
    /// 3. `Unary(Deref, expr)` on Box types → `FieldAccess(.value)`
    /// 4. `Local { index }` for address-taken → `FieldAccess(Local, .value)`
    /// 5. `Assign { target: Local, value }` for address-taken → assign to `.value`
    /// 6. `Assign { target: Deref(..), value }` for primitives → assign to `.value`
    /// 7. `VariantConstruct { Option, Some, primitive }` → wrap payload in Box
    fn transform_expr(
        &self,
        expr: &mut TirExpr,
        address_taken: &IndexSet<u32>,
        type_table: &TypeTable,
    ) {
        // Recursively transform sub-expressions first (bottom-up)
        match &mut expr.kind {
            TirExprKind::Binary { left, right, .. } => {
                self.transform_expr(left, address_taken, type_table);
                self.transform_expr(right, address_taken, type_table);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.transform_expr(arg, address_taken, type_table);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.transform_expr(receiver, address_taken, type_table);
                for arg in args {
                    self.transform_expr(arg, address_taken, type_table);
                }
            }
            TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.transform_expr(arg, address_taken, type_table);
                }
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::Index { expr: e, index, .. } => {
                self.transform_expr(e, address_taken, type_table);
                self.transform_expr(index, address_taken, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.transform_expr(condition, address_taken, type_table);
                self.transform_block(&mut *then_branch, address_taken, type_table);
                if let Some(else_branch) = else_branch {
                    self.transform_block(else_branch, address_taken, type_table);
                }
            }
            TirExprKind::Match { expr: e, arms } => {
                self.transform_expr(e, address_taken, type_table);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.transform_expr(guard, address_taken, type_table);
                    }
                    self.transform_expr(&mut arm.body, address_taken, type_table);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.transform_expr(&mut field.value, address_taken, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.transform_expr(elem, address_taken, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.transform_expr(body, address_taken, type_table);
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.transform_expr(callee, address_taken, type_table);
                for arg in args {
                    self.transform_expr(arg, address_taken, type_table);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.transform_expr(functor, address_taken, type_table);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload) = payload {
                    self.transform_expr(payload, address_taken, type_table);
                }
            }
            TirExprKind::VariantTag { expr: inner } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::VariantPayload { expr: inner, .. } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::VariantTest { expr: inner, .. } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.transform_expr(inner, address_taken, type_table);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.transform_expr(value, address_taken, type_table);
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.transform_expr(arg, address_taken, type_table);
                }
            }
            TirExprKind::Block(block) => {
                self.transform_block(block, address_taken, type_table);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.transform_block(block, address_taken, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.transform_expr(scrutinee, address_taken, type_table);
                for arm in arms {
                    self.transform_block(arm, address_taken, type_table);
                }
                self.transform_block(default, address_taken, type_table);
            }
            // Leaf nodes: no sub-expressions to transform
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Unit
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            // Assign and Unary are handled specially below (before recursion for some cases)
            TirExprKind::Assign { .. } | TirExprKind::Unary { .. } => {
                // Handled below
            }
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }

        // Now handle the boxing-specific transformations (top-down after sub-expressions)
        let span = expr.span;

        match &mut expr.kind {
            TirExprKind::Unary { op, expr: inner } => {
                // First recursively transform the inner expression
                // (need to handle address-taken locals BEFORE general Ref/Deref)
                self.transform_expr(inner, address_taken, type_table);

                match op {
                    TirUnaryOp::Ref | TirUnaryOp::MutRef => {
                        // Case 1: &local / &mut local where local is address-taken
                        // → just the local (the Box IS the reference)
                        if let TirExprKind::FieldAccess {
                            expr: box_local,
                            field_name,
                            ..
                        } = &inner.kind
                        {
                            // After address-taken local transformation, reads become
                            // FieldAccess(Local, .value). Taking a ref to that should
                            // just return the Box (the Local).
                            if field_name == "value"
                                && let TirExprKind::Local { index, .. } = &box_local.kind
                                && address_taken.contains(index)
                            {
                                let local_expr = (**box_local).clone();
                                *expr = local_expr;
                                return;
                            }
                        }

                        // Case 2: &primitive_expr / &mut primitive_expr
                        // → Box<T> { value: expr }
                        let inner_type_id = inner.type_id;
                        if let Some(&box_type_id) = self.box_struct_types.get(&inner_type_id) {
                            let box_struct_name = if let ResolvedType::Struct { name, .. } =
                                type_table.get(box_type_id)
                            {
                                name.clone()
                            } else {
                                panic!("Box type should be a struct");
                            };

                            let inner_owned = std::mem::replace(
                                inner.as_mut(),
                                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                            );

                            expr.kind = TirExprKind::StructLiteral {
                                struct_type: box_type_id,
                                struct_name: box_struct_name,
                                fields: vec![TirStructField {
                                    name: "value".to_string(),
                                    value: inner_owned,
                                    field_index: 0,
                                }],
                            };
                            expr.type_id = box_type_id;
                        }
                        // For non-primitive refs (structs, arrays, etc.), no change needed
                    }
                    TirUnaryOp::Deref => {
                        // Case 3: *ref_to_primitive → FieldAccess(.value)
                        // After type rewriting, ref types are Box<T> struct types
                        let inner_type_id = inner.type_id;
                        if self.box_type_ids.contains(&inner_type_id) {
                            let inner_type = self.get_box_inner_type(inner_type_id);
                            let result_type = inner_type.unwrap_or(expr.type_id);

                            let inner_owned = std::mem::replace(
                                inner.as_mut(),
                                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                            );

                            expr.kind = TirExprKind::FieldAccess {
                                expr: Box::new(inner_owned),
                                field_index: 0,
                                field_name: "value".to_string(),
                            };
                            expr.type_id = result_type;
                        }
                        // For non-primitive refs, Deref is a no-op in Wasm (transparent)
                    }
                    _ => {}
                } // Already handled sub-expression recursion
            }

            TirExprKind::Assign { target, value } => {
                self.transform_expr(value, address_taken, type_table);

                match &mut target.kind {
                    // Assign to address-taken local: x = val → x.value = val
                    TirExprKind::Local { index, name } => {
                        if address_taken.contains(index)
                            && self.box_struct_types.contains_key(&target.type_id)
                        {
                            let local_idx = *index;
                            let local_name = name.clone();
                            let box_type_id = *self
                                .box_struct_types
                                .get(&target.type_id)
                                .expect("address-taken local should have box type");
                            let local_expr = TirExpr::new(
                                TirExprKind::Local {
                                    index: local_idx,
                                    name: local_name,
                                },
                                box_type_id,
                                span,
                            );
                            target.kind = TirExprKind::FieldAccess {
                                expr: Box::new(local_expr),
                                field_index: 0,
                                field_name: "value".to_string(),
                            };
                            // target.type_id stays as the primitive type (the value's type)
                        } else {
                            self.transform_expr(target, address_taken, type_table);
                        }
                    }
                    // Assign through deref: *ref = val → ref.value = val
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: ref_expr,
                    } => {
                        self.transform_expr(ref_expr, address_taken, type_table);
                        let ref_type = ref_expr.type_id;
                        if self.box_type_ids.contains(&ref_type) {
                            let ref_owned = std::mem::replace(
                                ref_expr.as_mut(),
                                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                            );
                            let result_type =
                                self.get_box_inner_type(ref_type).unwrap_or(target.type_id);
                            target.kind = TirExprKind::FieldAccess {
                                expr: Box::new(ref_owned),
                                field_index: 0,
                                field_name: "value".to_string(),
                            };
                            target.type_id = result_type;
                        }
                    }
                    _ => {
                        self.transform_expr(target, address_taken, type_table);
                    }
                } // Already handled sub-expression recursion
            }

            TirExprKind::Local { index, name } => {
                if address_taken.contains(index) {
                    let original_type = expr.type_id;
                    if let Some(&box_type_id) = self.box_struct_types.get(&original_type) {
                        // Transform: Local { index } → FieldAccess(Local { index }, .value)
                        let local_expr = TirExpr::new(
                            TirExprKind::Local {
                                index: *index,
                                name: name.clone(),
                            },
                            box_type_id,
                            span,
                        );
                        expr.kind = TirExprKind::FieldAccess {
                            expr: Box::new(local_expr),
                            field_index: 0,
                            field_name: "value".to_string(),
                        };
                        // expr.type_id stays as the primitive type
                    }
                }
            }

            // Option now uses SubtypeHierarchy — no Box wrapping needed for
            // VariantConstruct("Some").
            _ => {}
        }
    }
}

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
    local_to_closure: IndexMap<u32, u32>,
    /// Closure IDs that can be specialized (stored in locals, called directly).
    /// Non-specializable closures use `ClosureToCanonical` for type-erased representation.
    specializable: IndexSet<u32>,
    /// Generated structs to add to module
    generated_structs: Vec<TirStruct>,
    /// Generated functions to add to module
    generated_functions: Vec<Rc<RefCell<TirFunction>>>,
    /// Map from fn-param spec key to specialized function name
    fn_param_specializations: IndexMap<FnParamSpecKey, String>,
}

impl ClosureLowerer {
    fn new(module_source: &ModuleSource) -> Self {
        Self {
            closure_counter: 0,
            module_source: module_source.clone(),
            collected_closures: Vec::new(),
            functor_infos: Vec::new(),
            local_to_closure: IndexMap::new(),
            specializable: IndexSet::new(),
            generated_structs: Vec::new(),
            generated_functions: Vec::new(),
            fn_param_specializations: IndexMap::new(),
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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
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
            TirStmtKind::LetPattern { value, .. } => {
                self.collect_closures_in_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn collect_closures_in_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Closure {
                params,
                body,
                captures,
                ..
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
            | TirExprKind::CmRawCall { args, .. }
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &arm.guard {
                        self.collect_closures_in_expr(guard);
                    }
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
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.collect_closures_in_expr(expr);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_closures_in_expr(scrutinee);
                for arm in arms {
                    self.collect_closures_in_block(arm);
                }
                self.collect_closures_in_block(default);
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }
    }

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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
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
            TirStmtKind::LetPattern { value, .. } => {
                self.analyze_closure_safety_expr(value, false);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
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
                    self.specializable.swap_remove(&closure_id);
                }

                // Recursively analyze the body
                self.analyze_closure_safety_expr(body, false);
            }
            TirExprKind::Local { index, .. } => {
                // If a local that holds a closure is passed as an argument, mark it unsafe
                if in_arg_position && let Some(closure_id) = self.local_to_closure.get(index) {
                    self.specializable.swap_remove(closure_id);
                }
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::CmRawCall { args, .. }
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &arm.guard {
                        self.analyze_closure_safety_expr(guard, false);
                    }
                    self.analyze_closure_safety_expr(&arm.body, false);
                }
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
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.analyze_closure_safety_expr(expr, false);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.analyze_closure_safety_expr(scrutinee, false);
                for arm in arms {
                    self.analyze_closure_safety_block(arm);
                }
                self.analyze_closure_safety_block(default);
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }
    }

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
                    is_pub: false,
                    type_id: cap.type_id,
                    index: i as u32,
                    span: collected.span,
                    is_hidden: false,
                    serde_rename: None,
                    serde_default: false,
                })
                .collect();

            let tir_struct = TirStruct {
                name: struct_name.clone(),
                is_pub: false,
                type_params: Vec::new(),
                monomorph_info: None,
                fields,
                span: collected.span,
                serde_rename_all: None,
            };
            self.generated_structs.push(tir_struct);

            // Generate __call method
            // Use a qualified name for the function to avoid collisions in the inliner's candidate map
            let simple_method_name = "__call".to_string();
            let qualified_method_name = MethodName::format_local(&struct_name, None, "__call");
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

            // Collect local types: self + params + internal locals from body
            // Parameters are locals 0 (self) through params.len()
            let param_count = 1 + collected.params.len() as u32;
            let mut local_types = vec![self_ref_type];
            local_types.extend(collected.params.iter().map(|(_, t)| *t));

            // Collect internal locals from the body (Let statements with index >= param_count)
            let mut body_locals: Vec<(u32, TypeId)> = Vec::new();
            Self::collect_locals_from_block(&body_block, &mut body_locals);

            // Extend local_types with body locals, sorted by index
            body_locals.sort_by_key(|(idx, _)| *idx);
            for (idx, type_id) in &body_locals {
                // Ensure we only add locals beyond parameter range
                if *idx >= param_count {
                    // Extend vector if needed to accommodate sparse indices
                    while local_types.len() <= *idx as usize {
                        local_types.push(TypeTable::UNKNOWN);
                    }
                    local_types[*idx as usize] = *type_id;
                }
            }

            // local_count is the total number of locals
            let local_count = local_types.len() as u32;

            // method_info tells codegen how to register this function with the proper mangled name
            let method_info = LocalMethodName::new(
                struct_name.clone(),        // __Closure_0
                None,                       // no trait
                simple_method_name.clone(), // __call (just the method name)
            );

            let call_method = TirFunction {
                is_async: false,
                name: qualified_method_name,
                is_pub: false,
                is_export: false, // Closure method, not a world export
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
                address_taken_locals: IndexSet::new(),
                is_cm_adapter: false,
                inline_hint: InlineHint::Auto,
                comp_features: 0,
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

    /// Collect all local variable declarations from a block, including nested blocks.
    /// Returns pairs of (`local_index`, `type_id`) for each Let statement found.
    fn collect_locals_from_block(block: &TirBlock, locals: &mut Vec<(u32, TypeId)>) {
        for stmt in &block.stmts {
            Self::collect_locals_from_stmt(stmt, locals);
        }
    }

    fn collect_locals_from_stmt(stmt: &TirStmt, locals: &mut Vec<(u32, TypeId)>) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                type_id,
                value,
                ..
            } => {
                locals.push((*local_index, *type_id));
                Self::collect_locals_from_expr(value, locals);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                Self::collect_locals_from_expr(expr, locals);
            }
            TirStmtKind::Return { value: None }
            | TirStmtKind::Break { .. }
            | TirStmtKind::Continue => {}
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::collect_locals_from_expr(condition, locals);
                Self::collect_locals_from_block(then_block, locals);
                if let Some(else_blk) = else_block {
                    Self::collect_locals_from_block(else_blk, locals);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                Self::collect_locals_from_block(body, locals);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                Self::collect_locals_from_expr(scrutinee, locals);
                Self::collect_locals_from_block(then_block, locals);
                if let Some(else_blk) = else_block {
                    Self::collect_locals_from_block(else_blk, locals);
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                Self::collect_locals_from_expr(value, locals);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn collect_locals_from_expr(expr: &TirExpr, locals: &mut Vec<(u32, TypeId)>) {
        match &expr.kind {
            TirExprKind::Block(block) => {
                Self::collect_locals_from_block(block, locals);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::collect_locals_from_expr(condition, locals);
                Self::collect_locals_from_block(then_branch, locals);
                if let Some(else_blk) = else_branch {
                    Self::collect_locals_from_block(else_blk, locals);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::collect_locals_from_expr(left, locals);
                Self::collect_locals_from_expr(right, locals);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::collect_locals_from_expr(inner, locals);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::StaticCall { args, .. }
            | TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    Self::collect_locals_from_expr(arg, locals);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::collect_locals_from_expr(receiver, locals);
                for arg in args {
                    Self::collect_locals_from_expr(arg, locals);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::collect_locals_from_expr(callee, locals);
                for arg in args {
                    Self::collect_locals_from_expr(arg, locals);
                }
            }
            TirExprKind::Index { expr: array, index } => {
                Self::collect_locals_from_expr(array, locals);
                Self::collect_locals_from_expr(index, locals);
            }
            TirExprKind::Assign { target, value } => {
                Self::collect_locals_from_expr(target, locals);
                Self::collect_locals_from_expr(value, locals);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::collect_locals_from_expr(&field.value, locals);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::collect_locals_from_expr(elem, locals);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_locals_from_expr(scrutinee, locals);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        Self::collect_locals_from_expr(guard, locals);
                    }
                    Self::collect_locals_from_expr(&arm.body, locals);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    Self::collect_locals_from_expr(payload_expr, locals);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                Self::collect_locals_from_block(block, locals);
            }
            // Terminals - no locals to collect
            _ => {}
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
            TirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => {
                let transformed_block = self.transform_closure_body_block(
                    block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                );
                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: label.clone(),
                        block: transformed_block,
                        result_type: *result_type,
                    },
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
            TirExprKind::StaticCall { func, args } => {
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
                    TirExprKind::StaticCall {
                        func: func.clone(),
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
                ..
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
                skip_value_copy: false,
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
            TirStmtKind::Loop { body } => TirStmtKind::Loop {
                body: self.transform_closure_body_block(
                    body,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
            },
            TirStmtKind::LabeledBlock { label, block } => TirStmtKind::LabeledBlock {
                label: label.clone(),
                block: self.transform_closure_body_block(
                    block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
            },
            TirStmtKind::IfPattern {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => TirStmtKind::IfPattern {
                scrutinee: self.transform_closure_body(
                    scrutinee,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
                pattern: self.transform_closure_body_pattern(pattern),
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
            TirStmtKind::LetPattern {
                pattern,
                is_mut,
                value,
            } => TirStmtKind::LetPattern {
                pattern: self.transform_closure_body_pattern(pattern),
                is_mut: *is_mut,
                value: self.transform_closure_body(
                    value,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
            },
            TirStmtKind::Break { label, value } => TirStmtKind::Break {
                label: label.clone(),
                value: value.as_ref().map(|v| {
                    self.transform_closure_body(v, captures, struct_type_id, self_ref_type, span)
                }),
            },
            TirStmtKind::Continue => TirStmtKind::Continue,
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        };
        TirStmt::new(kind, stmt.span)
    }

    /// Transform a pattern within a closure body, adjusting local indices by 1 for self parameter
    fn transform_closure_body_pattern(&self, pattern: &TirPattern) -> TirPattern {
        match pattern {
            TirPattern::Wildcard => TirPattern::Wildcard,
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => TirPattern::Binding {
                name: name.clone(),
                local_index: local_index + 1, // Shift by 1 for self parameter
                type_id: *type_id,
            },
            TirPattern::Literal(lit) => TirPattern::Literal(lit.clone()),
            TirPattern::Tuple(patterns) => TirPattern::Tuple(
                patterns
                    .iter()
                    .map(|p| self.transform_closure_body_pattern(p))
                    .collect(),
            ),
            TirPattern::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => TirPattern::Variant {
                enum_type: *enum_type,
                variant_name: variant_name.clone(),
                bindings: bindings
                    .iter()
                    .map(|p| self.transform_closure_body_pattern(p))
                    .collect(),
                payload_type: *payload_type,
            },
            TirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => TirPattern::Enum {
                enum_type: *enum_type,
                case_name: case_name.clone(),
                case_index: *case_index,
            },
            TirPattern::Struct {
                struct_type,
                fields,
                has_rest,
            } => TirPattern::Struct {
                struct_type: *struct_type,
                fields: fields
                    .iter()
                    .map(|f| crate::tir::TirStructPatternField {
                        field_name: f.field_name.clone(),
                        field_index: f.field_index,
                        pattern: self.transform_closure_body_pattern(&f.pattern),
                    })
                    .collect(),
                has_rest: *has_rest,
            },
        }
    }

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
        let mut func_by_name: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::new();
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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.fn_param_stored_in_struct_field(body, fn_param_indices)
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
            TirStmtKind::LetPattern { value, .. } => {
                self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
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
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                self.fn_param_in_struct_field_expr(inner, fn_param_indices)
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::StaticCall { args, .. }
            | TirExprKind::CmRawCall { args, .. } => args
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
            TirExprKind::TupleLiteral { elements } => elements
                .iter()
                .any(|e| self.fn_param_in_struct_field_expr(e, fn_param_indices)),
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
                    || arms.iter().any(|arm| {
                        arm.guard.as_ref().is_some_and(|g| {
                            self.fn_param_in_struct_field_expr(g, fn_param_indices)
                        }) || self.fn_param_in_struct_field_expr(&arm.body, fn_param_indices)
                    })
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
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.fn_param_in_struct_field_expr(expr, fn_param_indices)
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || arms
                        .iter()
                        .any(|arm| self.fn_param_stored_in_struct_field(arm, fn_param_indices))
                    || self.fn_param_stored_in_struct_field(default, fn_param_indices)
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }
    }

    /// Collect fn-param specialization requests from a block
    fn collect_fn_param_specs(
        &mut self,
        block: &TirBlock,
        func_by_name: &IndexMap<String, Rc<RefCell<TirFunction>>>,
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
        func_by_name: &IndexMap<String, Rc<RefCell<TirFunction>>>,
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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
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
            TirStmtKind::LetPattern { value, .. } => {
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn collect_fn_param_specs_expr(
        &mut self,
        expr: &TirExpr,
        func_by_name: &IndexMap<String, Rc<RefCell<TirFunction>>>,
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
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                self.collect_fn_param_specs_expr(inner, func_by_name, type_table, requests);
            }
            TirExprKind::CmRawCall { args, .. } => {
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &arm.guard {
                        self.collect_fn_param_specs_expr(guard, func_by_name, type_table, requests);
                    }
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
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.collect_fn_param_specs_expr(expr, func_by_name, type_table, requests);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                for arm in arms {
                    self.collect_fn_param_specs(arm, func_by_name, type_table, requests);
                }
                self.collect_fn_param_specs(default, func_by_name, type_table, requests);
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
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
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                self.count_closures_in_expr(inner, counter);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::StaticCall { args, .. }
            | TirExprKind::CmRawCall { args, .. } => {
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &arm.guard {
                        self.count_closures_in_expr(guard, counter);
                    }
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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
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
        let functor_suffix: String =
            key.functor_types
                .iter()
                .fold(String::new(), |mut acc, (_, tid)| {
                    let name = type_table.type_name(*tid);
                    let _ = write!(acc, "${name}");
                    acc
                });
        let specialized_name = format!("{}{}", callee.name, functor_suffix);

        // Build map from argument index to functor type
        // Note: key.functor_types contains argument indices (0 = first arg after receiver for methods)
        let arg_to_functor: IndexMap<u32, TypeId> = key.functor_types.iter().copied().collect();

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
        let local_to_functor: IndexMap<u32, TypeId> = arg_to_functor
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
                is_type_param_receiver: info.is_type_param_receiver,
            }
        });

        let specialized_func = TirFunction {
            is_async: false,
            name: specialized_name.clone(),
            is_pub: false,    // Specialized functions are always private
            is_export: false, // Specialized functions are not world exports
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
            is_cm_adapter: false,
            inline_hint: callee.inline_hint,
            comp_features: callee.comp_features,
        };

        self.generated_functions
            .push(Rc::new(RefCell::new(specialized_func)));
        specialized_name
    }

    /// Specialize a function body by transforming `IndirectCall` to `MethodCall` for fn-param locals
    fn specialize_function_body(
        &self,
        block: &TirBlock,
        param_to_functor: &IndexMap<u32, TypeId>,
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
        param_to_functor: &IndexMap<u32, TypeId>,
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
                ..
            } => TirStmtKind::Let {
                name: name.clone(),
                local_index: *local_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.specialize_expr(value, param_to_functor, type_table),
                skip_value_copy: false,
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
            TirStmtKind::Loop { body } => TirStmtKind::Loop {
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
            TirStmtKind::LetPattern {
                pattern,
                is_mut,
                value,
            } => TirStmtKind::LetPattern {
                pattern: pattern.clone(),
                is_mut: *is_mut,
                value: self.specialize_expr(value, param_to_functor, type_table),
            },
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        };
        TirStmt::new(kind, stmt.span)
    }

    /// When a fn-param local has been specialized to a functor type and is forwarded
    /// as an argument to another function expecting a function type, wrap it in
    /// `ClosureToCanonical` to convert the functor back to a type-erased canonical closure.
    ///
    /// `original_type_id` is the `type_id` of the argument BEFORE specialization
    /// (from the unmodified body), which retains the original function type.
    fn maybe_wrap_functor_as_canonical(
        &self,
        specialized_arg: TirExpr,
        original_type_id: TypeId,
        param_to_functor: &IndexMap<u32, TypeId>,
        type_table: &TypeTable,
    ) -> TirExpr {
        if let TirExprKind::Local { index, .. } = &specialized_arg.kind
            && let Some(&functor_type) = param_to_functor.get(index)
            && matches!(
                type_table.get(original_type_id),
                crate::tir::ResolvedType::Function { .. }
            )
        {
            // Find the functor_id by matching struct_type_id
            if let Some(functor) = self
                .functor_infos
                .iter()
                .find(|f| f.struct_type_id == functor_type)
            {
                let span = specialized_arg.span;
                return TirExpr::new(
                    TirExprKind::ClosureToCanonical {
                        functor: Box::new(specialized_arg),
                        functor_id: functor.id,
                        target_fn_type: original_type_id,
                    },
                    original_type_id,
                    span,
                );
            }
        }
        specialized_arg
    }

    fn specialize_expr(
        &self,
        expr: &TirExpr,
        param_to_functor: &IndexMap<u32, TypeId>,
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
                        let call_method_name =
                            MethodName::format_local(&functor.struct_name, None, "__call");

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
                        .map(|a| {
                            let original_type_id = a.type_id;
                            let specialized = self.specialize_expr(a, param_to_functor, type_table);
                            self.maybe_wrap_functor_as_canonical(
                                specialized,
                                original_type_id,
                                param_to_functor,
                                type_table,
                            )
                        })
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
                        .map(|a| {
                            let original_type_id = a.type_id;
                            let specialized = self.specialize_expr(a, param_to_functor, type_table);
                            self.maybe_wrap_functor_as_canonical(
                                specialized,
                                original_type_id,
                                param_to_functor,
                                type_table,
                            )
                        })
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
                        .map(|a| {
                            let original_type_id = a.type_id;
                            let specialized = self.specialize_expr(a, param_to_functor, type_table);
                            self.maybe_wrap_functor_as_canonical(
                                specialized,
                                original_type_id,
                                param_to_functor,
                                type_table,
                            )
                        })
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::CmRawCall { local_name, args } => TirExpr::new(
                TirExprKind::CmRawCall {
                    local_name: local_name.clone(),
                    args: args
                        .iter()
                        .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                        .collect(),
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
                            guard: arm
                                .guard
                                .as_ref()
                                .map(|g| self.specialize_expr(g, param_to_functor, type_table)),
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
                source_text,
            } => TirExpr::new(
                TirExprKind::Closure {
                    params: params.clone(),
                    body: Box::new(self.specialize_expr(body, param_to_functor, type_table)),
                    captures: captures.clone(),
                    functor_id: *functor_id,
                    source_text: source_text.clone(),
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
            TirExprKind::VariantTag { expr: inner } => TirExpr::new(
                TirExprKind::VariantTag {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantTest {
                expr: inner,
                case_index,
                case_name,
            } => TirExpr::new(
                TirExprKind::VariantTest {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    case_index: *case_index,
                    case_name: case_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantPayload {
                expr: inner,
                case_index,
                payload_type,
            } => TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => TirExpr::new(
                TirExprKind::Switch {
                    scrutinee: Box::new(self.specialize_expr(
                        scrutinee,
                        param_to_functor,
                        type_table,
                    )),
                    min_value: *min_value,
                    arms: arms
                        .iter()
                        .map(|arm| self.specialize_function_body(arm, param_to_functor, type_table))
                        .collect(),
                    default: self.specialize_function_body(default, param_to_functor, type_table),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }
    }

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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
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
            TirStmtKind::LetPattern { value, .. } => {
                self.transform_expr(value, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn transform_expr(&mut self, expr: &mut TirExpr, type_table: &mut TypeTable) {
        match &mut expr.kind {
            TirExprKind::Closure {
                body,
                captures,
                functor_id,
                ..
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
            TirExprKind::CmRawCall { args, .. } => {
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &mut arm.guard {
                        self.transform_expr(guard, type_table);
                    }
                    self.transform_expr(&mut arm.body, type_table);
                }
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
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.transform_expr(expr, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.transform_expr(scrutinee, type_table);
                for arm in arms {
                    self.transform_block(arm, type_table);
                }
                self.transform_block(default, type_table);
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
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
        let functor_suffix: String =
            functor_types
                .iter()
                .fold(String::new(), |mut acc, (_, tid)| {
                    let name = type_table.type_name(*tid);
                    let _ = write!(acc, "${name}");
                    acc
                });

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
                is_type_param_receiver: info.is_type_param_receiver,
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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
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
            TirStmtKind::LetPattern { value, .. } => {
                self.transform_remaining_closures_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
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
            | TirExprKind::CmRawCall { args, .. }
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
            | TirExprKind::FieldAccess { expr: inner, .. } => {
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &mut arm.guard {
                        self.transform_remaining_closures_expr(guard);
                    }
                    self.transform_remaining_closures_expr(&mut arm.body);
                }
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
            TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                self.transform_remaining_closures_expr(inner);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.transform_remaining_closures_expr(scrutinee);
                for arm in arms {
                    self.transform_remaining_closures_block(arm);
                }
                self.transform_remaining_closures_block(default);
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
        }
    }
}

/// Collects all string literals from a TIR module for the data section,
/// tracking which function each string comes from for DCE
struct StringCollector {
    strings: IndexSet<String>,
    /// Map of function name → strings in that function (for DCE filtering)
    function_strings: IndexMap<String, IndexSet<String>>,
    /// Map of function name → method info (for DCE to avoid parsing)
    function_method_info: IndexMap<String, Option<LocalMethodName>>,
    /// Current function being collected (for tracking)
    current_function: Option<String>,
}

impl StringCollector {
    fn new() -> Self {
        Self {
            strings: IndexSet::new(),
            function_strings: IndexMap::new(),
            function_method_info: IndexMap::new(),
            current_function: None,
        }
    }

    fn into_results(
        self,
    ) -> (
        Vec<String>,
        IndexMap<String, Vec<String>>,
        IndexMap<String, Option<LocalMethodName>>,
    ) {
        let strings = self.strings.into_iter().collect();
        let function_strings = self
            .function_strings
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect();
        (strings, function_strings, self.function_method_info)
    }

    fn add_string(&mut self, s: String) {
        self.strings.insert(s.clone());
        // Also track which function this string belongs to
        if let Some(func_name) = &self.current_function {
            self.function_strings
                .entry(func_name.clone())
                .or_default()
                .insert(s);
        }
    }

    fn collect_module(&mut self, module: &TirModule) {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.current_function = Some(func.name.clone());
                self.function_method_info
                    .insert(func.name.clone(), func.method_info.clone());
                self.collect_block(body);
                self.current_function = None;
            }
        }
        // Also collect from trait impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.current_function = Some(method.name.clone());
                    self.function_method_info
                        .insert(method.name.clone(), method.method_info.clone());
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
            TirStmtKind::Loop { body } => {
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
            TirStmtKind::LetPattern { value, .. } => {
                self.collect_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
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
            | TirExprKind::CmRawCall { args, .. }
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &arm.guard {
                        self.collect_expr(guard);
                    }
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
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_expr(payload_expr);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_expr(value);
            }
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.collect_expr(expr);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_expr(scrutinee);
                for arm in arms {
                    self.collect_block(arm);
                }
                self.collect_block(default);
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
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
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
