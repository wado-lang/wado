//! Store-to-Load Forwarding optimization for Wado TIR
//!
//! When a literal value is stored to a local variable and then loaded with no
//! intervening aliasing writes, this pass forwards the stored value directly,
//! eliminating the load. This is particularly useful after SROA decomposes
//! struct fields into scalar locals.
//!
//! Pattern handled:
//!
//! ```text
//! let mut x: i32 = 0;
//! x = 42;
//! let y = x;  // → let y = 42
//! ```
//!
//! Safety: A local is only eligible for forwarding when it cannot be modified
//! through aliasing. A local is excluded if:
//! - Its address is taken (`&x` or `&mut x`)
//! - It is captured by a closure
//!
//! At control flow boundaries, only locals modified within branches are
//! invalidated (selective invalidation), allowing known values for unmodified
//! locals to survive through assert branches and similar patterns.

use crate::hashmap::{IndexMap, IndexSet};
use crate::project::Project;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeTable,
};

/// A forwardable value: a scalar literal.
/// We only forward literals (not locals) to avoid complications with
/// mutable source locals being modified between the store and the load.
#[derive(Debug, Clone)]
enum ForwardValue {
    Int { value: u64, repr: String },
    Float { value: f64, repr: String },
    Bool(bool),
    Char(char),
}

impl ForwardValue {
    fn from_expr(expr: &TirExpr) -> Option<Self> {
        match &expr.kind {
            TirExprKind::IntLiteral { value, repr } => Some(Self::Int {
                value: *value,
                repr: repr.clone(),
            }),
            TirExprKind::FloatLiteral { value, repr } => Some(Self::Float {
                value: *value,
                repr: repr.clone(),
            }),
            TirExprKind::BoolLiteral(b) => Some(Self::Bool(*b)),
            TirExprKind::CharLiteral(c) => Some(Self::Char(*c)),
            _ => None,
        }
    }

    fn to_expr_kind(&self) -> TirExprKind {
        match self {
            Self::Int { value, repr } => TirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Float { value, repr } => TirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Bool(b) => TirExprKind::BoolLiteral(*b),
            Self::Char(c) => TirExprKind::CharLiteral(*c),
        }
    }
}

/// Known values state for forward propagation.
///
/// Only tracks whole-local scalar values, not individual fields.
/// Field forwarding is intentionally omitted because it requires alias
/// analysis to be correct: after `ref_elim`, `let r = x` creates a reference
/// alias, so writing `r.field` modifies the same object as `x.field`.
#[derive(Debug, Clone, Default)]
struct KnownValues {
    /// `local_index` → known literal value
    locals: IndexMap<u32, ForwardValue>,
}

impl KnownValues {
    fn set_local(&mut self, index: u32, value: ForwardValue) {
        self.locals.insert(index, value);
    }

    fn get_local(&self, index: u32) -> Option<&ForwardValue> {
        self.locals.get(&index)
    }

    fn invalidate_local(&mut self, index: u32) {
        self.locals.swap_remove(&index);
    }

    /// Invalidate only locals that appear in the modified set.
    fn invalidate_modified(&mut self, modified: &IndexSet<u32>) {
        for &idx in modified {
            self.invalidate_local(idx);
        }
    }
}

/// Collect all locals that are assigned within a block (including nested blocks).
fn collect_modified_locals(block: &TirBlock) -> IndexSet<u32> {
    let mut modified = IndexSet::default();
    for stmt in &block.stmts {
        collect_modified_in_stmt(stmt, &mut modified);
    }
    modified
}

fn collect_modified_in_stmt(stmt: &TirStmt, modified: &mut IndexSet<u32>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => collect_modified_in_expr(value, modified),
        TirStmtKind::Expr(expr) => collect_modified_in_expr(expr, modified),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_in_expr(v, modified);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_modified_in_expr(condition, modified);
            modified.extend(collect_modified_locals(then_block));
            if let Some(eb) = else_block {
                modified.extend(collect_modified_locals(eb));
            }
        }
        TirStmtKind::Loop { body } => {
            modified.extend(collect_modified_locals(body));
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            modified.extend(collect_modified_locals(block));
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_modified_in_expr(scrutinee, modified);
            modified.extend(collect_modified_locals(then_block));
            if let Some(eb) = else_block {
                modified.extend(collect_modified_locals(eb));
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_modified_in_expr(v, modified);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => collect_modified_in_expr(value, modified),
        TirStmtKind::TaskReturn { .. } => {}
    }
}

fn collect_modified_in_expr(expr: &TirExpr, modified: &mut IndexSet<u32>) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            if let TirExprKind::Local { index, .. } = &target.kind {
                modified.insert(*index);
            }
            if let TirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                modified.insert(*index);
            }
            collect_modified_in_expr(value, modified);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_modified_in_expr(left, modified);
            collect_modified_in_expr(right, modified);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => {
            collect_modified_in_expr(inner, modified);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_modified_in_expr(inner, modified);
            collect_modified_in_expr(index, modified);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_modified_in_expr(&arg.expr, modified);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_modified_in_expr(arg, modified);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_modified_in_expr(receiver, modified);
            for arg in args {
                collect_modified_in_expr(&arg.expr, modified);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_modified_in_expr(callee, modified);
            for arg in args {
                collect_modified_in_expr(arg, modified);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_modified_in_expr(functor, modified);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            modified.extend(collect_modified_locals(block));
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_modified_in_expr(condition, modified);
            modified.extend(collect_modified_locals(then_branch));
            if let Some(eb) = else_branch {
                modified.extend(collect_modified_locals(eb));
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_modified_in_expr(&field.value, modified);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_modified_in_expr(elem, modified);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_modified_in_expr(p, modified);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_modified_in_expr(body, modified);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_modified_in_expr(inner, modified);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_modified_in_expr(guard, modified);
                }
                collect_modified_in_expr(&arm.body, modified);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_modified_in_expr(value, modified);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            collect_modified_in_expr(expr, modified);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_modified_in_expr(scrutinee, modified);
            for arm in arms {
                modified.extend(collect_modified_locals(arm));
            }
            modified.extend(collect_modified_locals(default));
        }
        // Leaf nodes
        _ => {}
    }
}

/// Collect locals that are unsafe for forwarding:
/// - Address taken (& or &mut)
/// - Captured by a closure
/// - Assigned through dereference (*ptr = ...)
fn collect_unsafe_locals(body: &TirBlock) -> IndexSet<u32> {
    let mut unsafe_locals = IndexSet::default();
    collect_unsafe_in_block(body, &mut unsafe_locals);
    unsafe_locals
}

fn collect_unsafe_in_block(block: &TirBlock, unsafe_locals: &mut IndexSet<u32>) {
    for stmt in &block.stmts {
        collect_unsafe_in_stmt(stmt, unsafe_locals);
    }
}

fn collect_unsafe_in_stmt(stmt: &TirStmt, unsafe_locals: &mut IndexSet<u32>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => collect_unsafe_in_expr(value, unsafe_locals),
        TirStmtKind::Expr(expr) => collect_unsafe_in_expr(expr, unsafe_locals),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_unsafe_in_expr(v, unsafe_locals);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_unsafe_in_expr(condition, unsafe_locals);
            collect_unsafe_in_block(then_block, unsafe_locals);
            if let Some(eb) = else_block {
                collect_unsafe_in_block(eb, unsafe_locals);
            }
        }
        TirStmtKind::Loop { body } => collect_unsafe_in_block(body, unsafe_locals),
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_unsafe_in_block(block, unsafe_locals);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_unsafe_in_expr(scrutinee, unsafe_locals);
            collect_unsafe_in_block(then_block, unsafe_locals);
            if let Some(eb) = else_block {
                collect_unsafe_in_block(eb, unsafe_locals);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_unsafe_in_expr(v, unsafe_locals);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => collect_unsafe_in_expr(value, unsafe_locals),
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn collect_unsafe_in_expr(expr: &TirExpr, unsafe_locals: &mut IndexSet<u32>) {
    match &expr.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } => {
            // Address taken: mark the local as unsafe
            if let TirExprKind::Local { index, .. } = &inner.kind {
                unsafe_locals.insert(*index);
            }
            collect_unsafe_in_expr(inner, unsafe_locals);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_unsafe_in_expr(left, unsafe_locals);
            collect_unsafe_in_expr(right, unsafe_locals);
        }
        TirExprKind::Assign { target, value } => {
            collect_unsafe_in_expr(target, unsafe_locals);
            collect_unsafe_in_expr(value, unsafe_locals);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_unsafe_in_expr(&arg.expr, unsafe_locals);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_unsafe_in_expr(arg, unsafe_locals);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_unsafe_in_expr(receiver, unsafe_locals);
            for arg in args {
                collect_unsafe_in_expr(&arg.expr, unsafe_locals);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_unsafe_in_expr(callee, unsafe_locals);
            for arg in args {
                collect_unsafe_in_expr(arg, unsafe_locals);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_unsafe_in_expr(functor, unsafe_locals);
        }
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
            collect_unsafe_in_expr(index, unsafe_locals);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_unsafe_in_block(block, unsafe_locals);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_unsafe_in_expr(condition, unsafe_locals);
            collect_unsafe_in_block(then_branch, unsafe_locals);
            if let Some(eb) = else_branch {
                collect_unsafe_in_block(eb, unsafe_locals);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_unsafe_in_expr(&field.value, unsafe_locals);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_unsafe_in_expr(elem, unsafe_locals);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_unsafe_in_expr(p, unsafe_locals);
            }
        }
        TirExprKind::Closure { captures, body, .. } => {
            // All captured outer locals are unsafe
            for capture in captures {
                unsafe_locals.insert(capture.outer_index);
            }
            collect_unsafe_in_expr(body, unsafe_locals);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_unsafe_in_expr(guard, unsafe_locals);
                }
                collect_unsafe_in_expr(&arm.body, unsafe_locals);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_unsafe_in_expr(value, unsafe_locals);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            collect_unsafe_in_expr(expr, unsafe_locals);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_unsafe_in_expr(scrutinee, unsafe_locals);
            for arm in arms {
                collect_unsafe_in_block(arm, unsafe_locals);
            }
            collect_unsafe_in_block(default, unsafe_locals);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Apply store-to-load forwarding to all functions in the project.
pub fn forward_stores_to_loads(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= forward_in_function(&mut func, &type_table);
        }
    }
    changed
}

fn forward_in_function(func: &mut TirFunction, type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    // Phase 1: Collect unsafe locals (address taken, captured, etc.)
    let unsafe_locals = collect_unsafe_locals(body);

    // Phase 2: Forward known values
    let mut known = KnownValues::default();
    forward_in_block(body, &mut known, &unsafe_locals, type_table)
}

fn forward_in_block(
    block: &mut TirBlock,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= forward_in_stmt(stmt, known, unsafe_locals, type_table);
    }
    changed
}

fn forward_in_stmt(
    stmt: &mut TirStmt,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            let local_index = *local_index;

            // First, forward loads in the initializer expression
            let changed = forward_in_expr(value, known, unsafe_locals, type_table);

            // Then record known values from this let binding
            if !unsafe_locals.contains(&local_index)
                && let Some(fv) = ForwardValue::from_expr(value)
            {
                known.set_local(local_index, fv);
            }

            changed
        }
        TirStmtKind::Expr(expr) => {
            let changed = forward_in_expr(expr, known, unsafe_locals, type_table);
            // Check for assignments that update known values
            update_known_from_assign(expr, known, unsafe_locals);
            changed
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                forward_in_expr(v, known, unsafe_locals, type_table)
            } else {
                false
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = forward_in_expr(condition, known, unsafe_locals, type_table);
            // Collect locals modified in branches
            let mut modified = collect_modified_locals(then_block);
            if let Some(eb) = else_block.as_ref() {
                modified.extend(collect_modified_locals(eb));
            }
            // Forward into branches with cloned state
            let mut then_known = known.clone();
            changed |= forward_in_block(then_block, &mut then_known, unsafe_locals, type_table);
            if let Some(eb) = else_block {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, unsafe_locals, type_table);
            }
            // Only invalidate locals that were modified in branches
            known.invalidate_modified(&modified);
            changed
        }
        TirStmtKind::Loop { body } => {
            let modified = collect_modified_locals(body);
            known.invalidate_modified(&modified);
            let mut loop_known = known.clone();
            let changed = forward_in_block(body, &mut loop_known, unsafe_locals, type_table);
            known.invalidate_modified(&modified);
            changed
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            // Labeled blocks can `break` early, so we can't trust the final
            // known state for locals modified inside the block.
            let modified = collect_modified_locals(block);
            let changed = forward_in_block(block, known, unsafe_locals, type_table);
            // Invalidate only locals that were modified in the block
            known.invalidate_modified(&modified);
            changed
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = forward_in_expr(scrutinee, known, unsafe_locals, type_table);
            let mut modified = collect_modified_locals(then_block);
            if let Some(eb) = else_block.as_ref() {
                modified.extend(collect_modified_locals(eb));
            }
            let mut then_known = known.clone();
            changed |= forward_in_block(then_block, &mut then_known, unsafe_locals, type_table);
            if let Some(eb) = else_block {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, unsafe_locals, type_table);
            }
            known.invalidate_modified(&modified);
            changed
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                forward_in_expr(v, known, unsafe_locals, type_table)
            } else {
                false
            }
        }
        TirStmtKind::Continue => false,
        TirStmtKind::LetDestructure { value, .. } => {
            forward_in_expr(value, known, unsafe_locals, type_table)
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Forward known values within an expression tree.
fn forward_in_expr(
    expr: &mut TirExpr,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    let mut changed = false;

    // Try to forward this expression
    changed |= try_forward_expr(expr, known);

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= forward_in_expr(left, known, unsafe_locals, type_table);
            changed |= forward_in_expr(right, known, unsafe_locals, type_table);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table);
        }
        TirExprKind::Assign { target, value } => {
            changed |= forward_in_expr(value, known, unsafe_locals, type_table);
            // Update known state from this assignment
            update_known_from_target(target, value, known, unsafe_locals);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, unsafe_locals, type_table);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(arg, known, unsafe_locals, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= forward_in_expr(receiver, known, unsafe_locals, type_table);
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, unsafe_locals, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            changed |= forward_in_expr(callee, known, unsafe_locals, type_table);
            for arg in args {
                changed |= forward_in_expr(arg, known, unsafe_locals, type_table);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= forward_in_expr(functor, known, unsafe_locals, type_table);
        }
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table);
            changed |= forward_in_expr(index, known, unsafe_locals, type_table);
        }
        TirExprKind::Block(block) => {
            changed |= forward_in_block(block, known, unsafe_locals, type_table);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            // Labeled blocks in expression position can also break early
            let modified = collect_modified_locals(block);
            changed |= forward_in_block(block, known, unsafe_locals, type_table);
            known.invalidate_modified(&modified);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= forward_in_expr(condition, known, unsafe_locals, type_table);
            let mut modified = collect_modified_locals(then_branch);
            if let Some(eb) = else_branch.as_ref() {
                modified.extend(collect_modified_locals(eb));
            }
            let mut then_known = known.clone();
            changed |= forward_in_block(then_branch, &mut then_known, unsafe_locals, type_table);
            if let Some(eb) = else_branch {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, unsafe_locals, type_table);
            }
            known.invalidate_modified(&modified);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= forward_in_expr(&mut field.value, known, unsafe_locals, type_table);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                changed |= forward_in_expr(elem, known, unsafe_locals, type_table);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= forward_in_expr(p, known, unsafe_locals, type_table);
            }
        }
        TirExprKind::Closure { .. } => {
            // Don't propagate into closures
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table);
            let mut modified = IndexSet::default();
            for arm in arms.iter() {
                if let Some(guard) = &arm.guard {
                    collect_modified_in_expr(guard, &mut modified);
                }
                collect_modified_in_expr(&arm.body, &mut modified);
            }
            for arm in arms {
                let mut arm_known = known.clone();
                if let Some(guard) = &mut arm.guard {
                    changed |= forward_in_expr(guard, &mut arm_known, unsafe_locals, type_table);
                }
                changed |=
                    forward_in_expr(&mut arm.body, &mut arm_known, unsafe_locals, type_table);
            }
            known.invalidate_modified(&modified);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= forward_in_expr(value, known, unsafe_locals, type_table);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            changed |= forward_in_expr(expr, known, unsafe_locals, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= forward_in_expr(scrutinee, known, unsafe_locals, type_table);
            let mut modified = IndexSet::default();
            for arm in arms.iter() {
                modified.extend(collect_modified_locals(arm));
            }
            modified.extend(collect_modified_locals(default));
            for arm in arms {
                let mut arm_known = known.clone();
                changed |= forward_in_block(arm, &mut arm_known, unsafe_locals, type_table);
            }
            let mut default_known = known.clone();
            changed |= forward_in_block(default, &mut default_known, unsafe_locals, type_table);
            known.invalidate_modified(&modified);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }

    changed
}

/// Try to substitute a local read with a known literal.
fn try_forward_expr(expr: &mut TirExpr, known: &KnownValues) -> bool {
    if let TirExprKind::Local { index, .. } = &expr.kind
        && let Some(fv) = known.get_local(*index)
    {
        expr.kind = fv.to_expr_kind();
        return true;
    }
    false
}

/// Update known values from an assignment target.
fn update_known_from_target(
    target: &TirExpr,
    value: &TirExpr,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
) {
    if let TirExprKind::Local { index, .. } = &target.kind {
        let idx = *index;
        if unsafe_locals.contains(&idx) {
            return;
        }
        if let Some(fv) = ForwardValue::from_expr(value) {
            known.set_local(idx, fv);
        } else {
            known.invalidate_local(idx);
        }
    }
}

/// Update known values from an Expr statement that is an assignment.
fn update_known_from_assign(
    expr: &TirExpr,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
) {
    if let TirExprKind::Assign { target, value } = &expr.kind {
        update_known_from_target(target, value, known, unsafe_locals);
    }
}
