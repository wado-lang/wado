//! Copy propagation optimization for Wado NIR.
//!
//! This module eliminates trivial copy bindings like `let x = y`, `let x = 42`,
//! `let x = &y`, or `let x = &mut y` by propagating the source value to all
//! uses of the target variable.
//!
//! The optimization is safe when:
//! - The target variable is not assigned after initialization
//! - The target variable does not have its address taken
//! - For local-to-local copies: the source is not modified after the copy
//! - For value types: the source is dead after the binding (`read_count` is 1)
//! - For ref/mut-ref copies: the target is single-use and the source is not reassigned
//!
//! Closure captures: NIR materialises captures into `NirCapture` entries on
//! a separate `ClosureFunctor`, evaluated at functor construction time.
//! Substituting a copy-bound local's source through a `ClosureToCanonical`
//! literal evaluates the source at the same program point the original
//! local would have been read, so no closure-capture safety gate is
//! needed.

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

/// Information about a copy binding that may be eliminable.
/// Pattern: `let x: T = y` where y is a local variable, simple literal, or `&y`/`&mut y`
#[derive(Debug, Clone)]
struct CopyBinding {
    /// Local index of the target variable (x)
    target_local: u32,
    /// The source expression (either a Local, simple literal, or Ref/MutRef of a Local)
    source: CopySource,
    /// Type of the binding
    type_id: TypeId,
}

/// Source of a copy binding
#[derive(Debug, Clone)]
enum CopySource {
    /// Copy from another local variable
    Local { index: u32, name: String },
    /// Copy from an integer literal
    IntLiteral { value: u64, repr: String },
    /// Copy from a float literal
    FloatLiteral { value: f64, repr: String },
    /// Copy from a bool literal
    BoolLiteral(bool),
    /// Copy from a char literal
    CharLiteral(char),
    /// Copy from `&local` — propagated as `&source` at use site
    Ref {
        index: u32,
        name: String,
        inner_type_id: TypeId,
    },
    /// Copy from `&mut local` — propagated as `&mut source` at use site
    MutRef {
        index: u32,
        name: String,
        inner_type_id: TypeId,
    },
}

/// Usage information for a local variable
#[derive(Debug, Default)]
struct LocalUsage {
    /// Number of times the local is read
    read_count: u32,
    /// Whether the local is ever assigned to (after initialization)
    is_assigned: bool,
    /// Whether a field of this local is ever assigned (e.g., `x.field = ...`),
    /// or whether a `&mut` reference to the local was taken (which may mutate
    /// the local through the reference, e.g. after inlining an `&mut self` method).
    has_field_mutation: bool,
    /// Whether the local has its address taken
    address_taken: bool,
}

/// If `expr` is `builtin::copy_value::<T>(inner)`, return `inner`; otherwise
/// return `expr` unchanged. Copy propagation treats the wrapper as
/// transparent because its safety check at `can_propagate_copy` already
/// refuses to eliminate bindings whose target receives field mutations on a
/// value-semantic type — i.e. the cases where dropping the copy would be
/// observable.
fn unwrap_copy_value(expr: &NirExpr) -> &NirExpr {
    if let NirExprKind::Call { func, args, .. } = &expr.kind
        && func.module_source.is_core_builtin()
        && func.name == "copy_value"
        && args.len() == 1
    {
        return &args[0].expr;
    }
    expr
}

/// Analyze a Let statement to see if it's a copy binding.
fn analyze_copy_binding(stmt: &NirStmt) -> Option<CopyBinding> {
    let NirStmtKind::Let {
        local_index,
        value,
        skip_value_copy,
        ..
    } = &stmt.kind
    else {
        return None;
    };

    // skip_value_copy bindings (from tmpl_hoist, SROA, LICM, etc.) intentionally
    // alias a hoisted buffer. Eliminating them would break that aliasing contract.
    if *skip_value_copy {
        return None;
    }

    // Look through `builtin::copy_value::<T>(x)` wrappers: when the safety
    // check later in `can_propagate_copy` rules the binding eliminable, the
    // semantic copy is also unnecessary so both go away together.
    let value = unwrap_copy_value(value);

    let source = match &value.kind {
        NirExprKind::Local { index, name } => CopySource::Local {
            index: *index,
            name: name.clone(),
        },
        NirExprKind::IntLiteral { value, repr } => CopySource::IntLiteral {
            value: *value,
            repr: repr.clone(),
        },
        NirExprKind::FloatLiteral { value, repr } => CopySource::FloatLiteral {
            value: *value,
            repr: repr.clone(),
        },
        NirExprKind::BoolLiteral(b) => CopySource::BoolLiteral(*b),
        NirExprKind::CharLiteral(c) => CopySource::CharLiteral(*c),
        // `let x = &y` or `let x = &mut y` where y is a local
        NirExprKind::Unary { op, expr: inner }
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef) =>
        {
            if let NirExprKind::Local { index, name } = &inner.kind {
                if matches!(op, NirUnaryOp::Ref) {
                    CopySource::Ref {
                        index: *index,
                        name: name.clone(),
                        inner_type_id: inner.type_id,
                    }
                } else {
                    CopySource::MutRef {
                        index: *index,
                        name: name.clone(),
                        inner_type_id: inner.type_id,
                    }
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    Some(CopyBinding {
        target_local: *local_index,
        source,
        type_id: value.type_id,
    })
}

/// Combined analysis: collect both copy bindings and local usage in a single tree walk.
struct AnalysisResult {
    bindings: Vec<CopyBinding>,
    usage: IndexMap<u32, LocalUsage>,
}

/// Map from `(ModuleSource, function_name)` to the first parameter's `TypeId`.
/// Used to determine whether a non-inlined method call mutates its receiver
/// (i.e., whether the first param is `&mut T` even when the receiver expression
/// has type `T`).
type FirstParamTypes = IndexMap<(ModuleSource, String), TypeId>;

fn analyze_function_body(
    body: &NirBlock,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) -> AnalysisResult {
    let mut result = AnalysisResult {
        bindings: Vec::new(),
        usage: IndexMap::default(),
    };
    analyze_block(body, &mut result, type_table, first_param_types);
    result
}

fn analyze_block(
    block: &NirBlock,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) {
    for stmt in &block.stmts {
        // Check for copy bindings at statement level
        if let Some(binding) = analyze_copy_binding(stmt) {
            result.bindings.push(binding);
        }
        analyze_stmt(stmt, result, type_table, first_param_types);
    }
}

fn analyze_stmt(
    stmt: &NirStmt,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => {
            analyze_expr(value, result, type_table, first_param_types);
        }
        NirStmtKind::Expr(expr) => {
            analyze_expr(expr, result, type_table, first_param_types);
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                analyze_expr(v, result, type_table, first_param_types);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            analyze_expr(condition, result, type_table, first_param_types);
            analyze_block(then_block, result, type_table, first_param_types);
            if let Some(eb) = else_block {
                analyze_block(eb, result, type_table, first_param_types);
            }
        }
        NirStmtKind::Loop { body } => {
            analyze_block(body, result, type_table, first_param_types);
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            analyze_block(block, result, type_table, first_param_types);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                analyze_expr(v, result, type_table, first_param_types);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => {
            analyze_expr(value, result, type_table, first_param_types);
        }
    }
}

fn analyze_expr(
    expr: &NirExpr,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            result.usage.entry(*index).or_default().read_count += 1;
        }
        NirExprKind::Assign { target, value } => {
            if let NirExprKind::Local { index, .. } = &target.kind {
                result.usage.entry(*index).or_default().is_assigned = true;
            }
            // Track field mutations: `x.field = ...` where x is a local
            if let NirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                && let NirExprKind::Local { index, .. } = &inner.kind
            {
                result.usage.entry(*index).or_default().has_field_mutation = true;
            }
            analyze_expr(target, result, type_table, first_param_types);
            analyze_expr(value, result, type_table, first_param_types);
        }
        NirExprKind::Unary { op, expr: inner } => {
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && let NirExprKind::Local { index, .. } = &inner.kind
            {
                result.usage.entry(*index).or_default().address_taken = true;
                // Taking `&mut x` means `x` may be mutated through the reference
                // (e.g. after inlining an `&mut self` method, the inlined body
                // contains `let self = &mut c` — the mutation of `*self` is not
                // visible as a direct field write on `c`, so we use the MutRef
                // itself as the mutation signal).
                if matches!(op, NirUnaryOp::MutRef) {
                    result.usage.entry(*index).or_default().has_field_mutation = true;
                }
            }
            analyze_expr(inner, result, type_table, first_param_types);
        }
        NirExprKind::Binary { left, right, .. } => {
            analyze_expr(left, result, type_table, first_param_types);
            analyze_expr(right, result, type_table, first_param_types);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                if arg.is_mut && may_mutate_through_arg(&arg.expr, type_table) {
                    mark_potentially_mutated_local(&arg.expr, result);
                }
                analyze_expr(&arg.expr, result, type_table, first_param_types);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                analyze_expr(arg, result, type_table, first_param_types);
            }
        }
        NirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            // A method call mutates its receiver when the method's first parameter
            // is `&mut Self`. In NIR, auto-ref is implicit: the receiver expression
            // has type `T` even when the method is declared as `fn f(&mut self)`.
            // Check both the receiver type (already `&mut T`) and the function's
            // first param type (for the case where receiver is still plain `T`).
            let receiver_is_mut_ref = may_mutate_caller_state(receiver, type_table);
            let func_first_param_is_mut_ref = first_param_types
                .get(&(func.module_source.clone(), func.name.clone()))
                .is_some_and(|&tp| matches!(type_table.get(tp), ResolvedType::MutRef(_)));
            if receiver_is_mut_ref || func_first_param_is_mut_ref {
                mark_potentially_mutated_local(receiver, result);
            }
            analyze_expr(receiver, result, type_table, first_param_types);
            for arg in args {
                if arg.is_mut && may_mutate_through_arg(&arg.expr, type_table) {
                    mark_potentially_mutated_local(&arg.expr, result);
                }
                analyze_expr(&arg.expr, result, type_table, first_param_types);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            analyze_expr(callee, result, type_table, first_param_types);
            for arg in args {
                analyze_expr(arg, result, type_table, first_param_types);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            analyze_expr(functor, result, type_table, first_param_types);
        }
        NirExprKind::FieldAccess { expr: inner, .. } => {
            analyze_expr(inner, result, type_table, first_param_types);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            analyze_expr(inner, result, type_table, first_param_types);
            analyze_expr(index, result, type_table, first_param_types);
        }
        NirExprKind::Cast { expr: inner, .. } => {
            analyze_expr(inner, result, type_table, first_param_types);
        }
        NirExprKind::Block(block) => {
            analyze_block(block, result, type_table, first_param_types);
        }
        NirExprKind::LabeledBlock { block, .. } => {
            analyze_block(block, result, type_table, first_param_types);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_expr(condition, result, type_table, first_param_types);
            analyze_block(then_branch, result, type_table, first_param_types);
            if let Some(eb) = else_branch {
                analyze_block(eb, result, type_table, first_param_types);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_expr(&field.value, result, type_table, first_param_types);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } | NirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                analyze_expr(elem, result, type_table, first_param_types);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                analyze_expr(payload_expr, result, type_table, first_param_types);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            analyze_expr(inner, result, type_table, first_param_types);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_expr(guard, result, type_table, first_param_types);
                }
                analyze_expr(&arm.body, result, type_table, first_param_types);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            analyze_expr(value, result, type_table, first_param_types);
        }
        NirExprKind::VariantTag { expr } | NirExprKind::VariantTest { expr, .. } => {
            analyze_expr(expr, result, type_table, first_param_types);
        }
        NirExprKind::VariantPayload { expr, .. } => {
            analyze_expr(expr, result, type_table, first_param_types);
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_expr(scrutinee, result, type_table, first_param_types);
            for arm in arms {
                analyze_block(arm, result, type_table, first_param_types);
            }
            analyze_block(default, result, type_table, first_param_types);
        }
        // Leaf nodes
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

fn mark_potentially_mutated_local(expr: &NirExpr, result: &mut AnalysisResult) {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            result.usage.entry(*index).or_default().has_field_mutation = true;
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. } => {
            mark_potentially_mutated_local(inner, result);
        }
        NirExprKind::Index { expr: inner, .. } => {
            mark_potentially_mutated_local(inner, result);
        }
        _ => {}
    }
}

fn may_mutate_caller_state(expr: &NirExpr, type_table: &TypeTable) -> bool {
    matches!(type_table.get(expr.type_id), ResolvedType::MutRef(_))
}

fn may_mutate_through_arg(expr: &NirExpr, type_table: &TypeTable) -> bool {
    matches!(type_table.get(expr.type_id), ResolvedType::MutRef(_))
}

/// Check if a type requires value copying (composite types with value semantics).
fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
        // References, primitives, etc. don't need copying
        _ => false,
    }
}

/// Check if a binding can be safely eliminated via copy propagation.
fn can_propagate_copy(
    binding: &CopyBinding,
    usage: &IndexMap<u32, LocalUsage>,
    type_table: &TypeTable,
) -> bool {
    let target_usage = usage.get(&binding.target_local);

    // If the target is never used, it can be eliminated (dead code)
    let Some(target_usage) = target_usage else {
        return true;
    };

    // Don't propagate if target is assigned to after initialization
    if target_usage.is_assigned {
        return false;
    }

    // Don't propagate if target has field mutations and the type needs value copy.
    // In Wasm GC, `let q = p` for a struct type creates a reference alias, not a
    // value copy. The WIR translator inserts value_copy for mutable Let bindings.
    // Eliminating the binding would lose this copy, causing field mutations on the
    // target to incorrectly affect the source.
    if target_usage.has_field_mutation && needs_value_copy(binding.type_id, type_table) {
        return false;
    }

    // Don't propagate if address is taken (could be modified through pointer)
    if target_usage.address_taken {
        return false;
    }

    match &binding.source {
        CopySource::Local { index, .. } => {
            // For local-to-local copy:
            // Safe if source is not modified after the copy
            let source_usage = usage.get(index);
            if let Some(su) = source_usage {
                // Source is assigned after initialization - not safe to propagate
                // because uses of the target should see the pre-assignment value
                if su.is_assigned {
                    return false;
                }
            }

            let is_value_type = needs_value_copy(binding.type_id, type_table);

            if is_value_type
                && target_usage.read_count == 1
                && let Some(su) = source_usage
                && su.has_field_mutation
            {
                return false;
            }

            // For value-type copies where the target is used exactly once,
            // we can safely propagate even when the source's address was taken.
            // The source is not reassigned (checked above), so reading it at the
            // target's single use point yields the same value as the snapshot.
            // This enables elimination of inlined builder patterns like:
            //   __inline_build: { let self = __b; break: *self; }
            let single_use_value_copy = is_value_type && target_usage.read_count == 1;

            if let Some(su) = source_usage
                && !single_use_value_copy
                && su.address_taken
            {
                // Source could be modified through a reference.
                return false;
            }

            // For value types (structs, arrays, tuples, strings), only propagate
            // if the source is dead after this binding (read_count == 1),
            // unless the target is single-use (already verified safe above).
            if is_value_type
                && !single_use_value_copy
                && let Some(su) = source_usage
                && (su.read_count > 1 || su.address_taken)
            {
                return false;
            }
            // If no usage info, source is unused - safe to eliminate

            true
        }
        // Literals are always safe to propagate
        CopySource::IntLiteral { .. }
        | CopySource::FloatLiteral { .. }
        | CopySource::BoolLiteral(_)
        | CopySource::CharLiteral(_) => true,
        // `let x = &y` or `let x = &mut y`:
        // Safe when target is single-use and source is not reassigned.
        // We move the `&y`/`&mut y` expression from the definition to the use site.
        // Multi-use is not allowed because each substitution creates a new `&mut`
        // expression, which could change semantics in the presence of aliasing.
        CopySource::Ref { index, .. } | CopySource::MutRef { index, .. } => {
            // Must be single-use
            if target_usage.read_count != 1 {
                return false;
            }
            // Source must not be reassigned
            if let Some(su) = usage.get(index)
                && su.is_assigned
            {
                return false;
            }
            true
        }
    }
}

/// Combined substitute and remove: replaces local references and removes dead Let stmts in a single walk.
fn apply_in_block(
    block: &mut NirBlock,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    block.stmts.retain(|stmt| {
        if let NirStmtKind::Let { local_index, .. } = &stmt.kind {
            !dead_locals.contains(local_index)
        } else {
            true
        }
    });
    for stmt in &mut block.stmts {
        apply_in_stmt(stmt, substitutions, dead_locals);
    }
}

fn apply_in_stmt(
    stmt: &mut NirStmt,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            apply_in_expr(value, substitutions, dead_locals);
        }
        NirStmtKind::Expr(expr) => {
            apply_in_expr(expr, substitutions, dead_locals);
        }
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                apply_in_expr(v, substitutions, dead_locals);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            apply_in_expr(condition, substitutions, dead_locals);
            apply_in_block(then_block, substitutions, dead_locals);
            if let Some(eb) = else_block {
                apply_in_block(eb, substitutions, dead_locals);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            apply_in_block(body, substitutions, dead_locals);
        }
        NirStmtKind::Continue => {}
    }
}

fn apply_in_expr(
    expr: &mut NirExpr,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    if let NirExprKind::Local { index, .. } = &expr.kind
        && let Some(source) = substitutions.get(index)
    {
        match source {
            CopySource::Local {
                index: src_idx,
                name: src_name,
            } => {
                expr.kind = NirExprKind::Local {
                    index: *src_idx,
                    name: src_name.clone(),
                };
            }
            CopySource::IntLiteral { value, repr } => {
                expr.kind = NirExprKind::IntLiteral {
                    value: *value,
                    repr: repr.clone(),
                };
            }
            CopySource::FloatLiteral { value, repr } => {
                expr.kind = NirExprKind::FloatLiteral {
                    value: *value,
                    repr: repr.clone(),
                };
            }
            CopySource::BoolLiteral(b) => {
                expr.kind = NirExprKind::BoolLiteral(*b);
            }
            CopySource::CharLiteral(c) => {
                expr.kind = NirExprKind::CharLiteral(*c);
            }
            CopySource::Ref {
                index: src_idx,
                name: src_name,
                inner_type_id,
            }
            | CopySource::MutRef {
                index: src_idx,
                name: src_name,
                inner_type_id,
            } => {
                let op = if matches!(source, CopySource::Ref { .. }) {
                    NirUnaryOp::Ref
                } else {
                    NirUnaryOp::MutRef
                };
                let inner = NirExpr::new(
                    NirExprKind::Local {
                        index: *src_idx,
                        name: src_name.clone(),
                    },
                    *inner_type_id,
                    expr.span,
                );
                expr.kind = NirExprKind::Unary {
                    op,
                    expr: Box::new(inner),
                };
            }
        }
        return;
    }

    match &mut expr.kind {
        NirExprKind::Local { .. } => {}
        NirExprKind::Binary { left, right, .. } => {
            apply_in_expr(left, substitutions, dead_locals);
            apply_in_expr(right, substitutions, dead_locals);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. } => {
            apply_in_expr(inner, substitutions, dead_locals);
        }
        NirExprKind::Assign { target, value } => {
            apply_in_expr(target, substitutions, dead_locals);
            apply_in_expr(value, substitutions, dead_locals);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            apply_in_expr(inner, substitutions, dead_locals);
            apply_in_expr(index, substitutions, dead_locals);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                apply_in_expr(&mut arg.expr, substitutions, dead_locals);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                apply_in_expr(arg, substitutions, dead_locals);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            apply_in_expr(receiver, substitutions, dead_locals);
            for arg in args {
                apply_in_expr(&mut arg.expr, substitutions, dead_locals);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            apply_in_expr(callee, substitutions, dead_locals);
            for arg in args {
                apply_in_expr(arg, substitutions, dead_locals);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            apply_in_expr(functor, substitutions, dead_locals);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            apply_in_block(block, substitutions, dead_locals);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            apply_in_expr(condition, substitutions, dead_locals);
            apply_in_block(then_branch, substitutions, dead_locals);
            if let Some(eb) = else_branch {
                apply_in_block(eb, substitutions, dead_locals);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                apply_in_expr(&mut field.value, substitutions, dead_locals);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } | NirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                apply_in_expr(elem, substitutions, dead_locals);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                apply_in_expr(p, substitutions, dead_locals);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            apply_in_expr(inner, substitutions, dead_locals);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    apply_in_expr(guard, substitutions, dead_locals);
                }
                apply_in_expr(&mut arm.body, substitutions, dead_locals);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            apply_in_expr(value, substitutions, dead_locals);
        }
        NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. } => {
            apply_in_expr(expr, substitutions, dead_locals);
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            apply_in_expr(scrutinee, substitutions, dead_locals);
            for arm in arms {
                apply_in_block(arm, substitutions, dead_locals);
            }
            apply_in_block(default, substitutions, dead_locals);
        }
        // Leaf nodes
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Eliminate trivial copy bindings in a function.
/// Uses merged analysis (1 walk) and merged substitute+remove (1 walk) per iteration.
fn propagate_copies_in_function(
    func: &mut NirFunction,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) -> bool {
    let Some(mut owned) = func.body_block() else {
        return false;
    };
    let body = &mut owned;

    let mut ever_changed = false;

    loop {
        // Step 1: Collect copy bindings and usage info in a single walk
        let analysis = analyze_function_body(body, type_table, first_param_types);

        if analysis.bindings.is_empty() {
            break;
        }

        // Step 2: Find all eliminable bindings
        let eliminable: Vec<CopyBinding> = analysis
            .bindings
            .into_iter()
            .filter(|b| can_propagate_copy(b, &analysis.usage, type_table))
            .collect();

        if eliminable.is_empty() {
            break;
        }

        // Step 3: Build non-conflicting batch.
        let target_set: IndexSet<u32> = eliminable.iter().map(|b| b.target_local).collect();

        let mut substitutions: IndexMap<u32, CopySource> = IndexMap::default();
        let mut has_deferred = false;

        for binding in eliminable {
            let source_conflicts = match &binding.source {
                CopySource::Local { index, .. }
                | CopySource::Ref { index, .. }
                | CopySource::MutRef { index, .. } => target_set.contains(index),
                _ => false,
            };
            if source_conflicts {
                has_deferred = true;
            } else {
                substitutions.insert(binding.target_local, binding.source);
            }
        }

        if substitutions.is_empty() {
            break;
        }

        // Step 4: Apply substitutions and remove dead bindings in a single walk
        let dead_locals: IndexSet<u32> = substitutions.keys().copied().collect();
        apply_in_block(body, &substitutions, &dead_locals);
        ever_changed = true;

        if !has_deferred {
            break;
        }
    }

    func.set_body_block(owned);
    ever_changed
}

/// Apply copy propagation to all functions in the project.
pub fn propagate_copies(project: &mut NirPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    // Build a map from (module_source, function_name) → first-param TypeId so that
    // MethodCall analysis can determine whether a call with an implicit-ref receiver
    // (e.g. `c.increment()` where `increment` takes `&mut self`) mutates the receiver.
    let mut first_param_types: FirstParamTypes = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(first_param) = func.params.first() {
            let key = (func.module_source.clone(), func.name.clone());
            first_param_types.insert(key, first_param.type_id);
        }
    }
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= propagate_copies_in_function(&mut func, &type_table, &first_param_types);
    }
    changed
}
