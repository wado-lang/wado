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

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeTable,
};

/// Precomputed modified-locals cache, keyed by block raw pointer.
///
/// Built in a single O(n) bottom-up pass before the forward traversal,
/// replacing the previous O(n²) approach where `collect_modified_locals`
/// was called inline at every control-flow node (re-walking subtrees).
type ModifiedLocalsCache = IndexMap<*const TirBlock, IndexSet<u32>>;

/// Precompute modified locals for all blocks in a single bottom-up pass.
fn precompute_all_modified_locals(body: &TirBlock) -> ModifiedLocalsCache {
    let mut cache = ModifiedLocalsCache::default();
    precompute_modified_block(body, &mut cache);
    cache
}

/// Returns the modified set for this block, also inserting it into the cache.
fn precompute_modified_block(block: &TirBlock, cache: &mut ModifiedLocalsCache) -> IndexSet<u32> {
    let mut modified = IndexSet::default();
    for stmt in &block.stmts {
        precompute_modified_stmt(stmt, &mut modified, cache);
    }
    cache.insert(std::ptr::from_ref::<TirBlock>(block), modified.clone());
    modified
}

fn precompute_modified_stmt(
    stmt: &TirStmt,
    modified: &mut IndexSet<u32>,
    cache: &mut ModifiedLocalsCache,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => precompute_modified_expr(value, modified, cache),
        TirStmtKind::Expr(expr) => precompute_modified_expr(expr, modified, cache),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                precompute_modified_expr(v, modified, cache);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            precompute_modified_expr(condition, modified, cache);
            modified.extend(precompute_modified_block(then_block, cache));
            if let Some(eb) = else_block {
                modified.extend(precompute_modified_block(eb, cache));
            }
        }
        TirStmtKind::Loop { body } => {
            modified.extend(precompute_modified_block(body, cache));
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            modified.extend(precompute_modified_block(block, cache));
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            precompute_modified_expr(scrutinee, modified, cache);
            modified.extend(precompute_modified_block(then_block, cache));
            if let Some(eb) = else_block {
                modified.extend(precompute_modified_block(eb, cache));
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                precompute_modified_expr(v, modified, cache);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => {
            precompute_modified_expr(value, modified, cache);
        }
        TirStmtKind::TaskReturn { .. } => {}
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn precompute_modified_expr(
    expr: &TirExpr,
    modified: &mut IndexSet<u32>,
    cache: &mut ModifiedLocalsCache,
) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            if let TirExprKind::Local { index, .. } = &target.kind {
                modified.insert(*index);
            }
            if let TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } = &target.kind
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                modified.insert(*index);
            }
            precompute_modified_expr(value, modified, cache);
        }
        TirExprKind::Binary { left, right, .. } => {
            precompute_modified_expr(left, modified, cache);
            precompute_modified_expr(right, modified, cache);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::Cast { expr: inner, .. } => {
            precompute_modified_expr(inner, modified, cache);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            precompute_modified_expr(inner, modified, cache);
            precompute_modified_expr(index, modified, cache);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                precompute_modified_expr(&arg.expr, modified, cache);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                precompute_modified_expr(arg, modified, cache);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            precompute_modified_expr(receiver, modified, cache);
            for arg in args {
                precompute_modified_expr(&arg.expr, modified, cache);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            precompute_modified_expr(callee, modified, cache);
            for arg in args {
                precompute_modified_expr(arg, modified, cache);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            precompute_modified_expr(functor, modified, cache);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            modified.extend(precompute_modified_block(block, cache));
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            precompute_modified_expr(condition, modified, cache);
            modified.extend(precompute_modified_block(then_branch, cache));
            if let Some(eb) = else_branch {
                modified.extend(precompute_modified_block(eb, cache));
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                precompute_modified_expr(&field.value, modified, cache);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                precompute_modified_expr(elem, modified, cache);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                precompute_modified_expr(p, modified, cache);
            }
        }
        TirExprKind::Closure { body, .. } => {
            precompute_modified_expr(body, modified, cache);
        }
        TirExprKind::Match { expr: inner, arms } => {
            precompute_modified_expr(inner, modified, cache);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    precompute_modified_expr(guard, modified, cache);
                }
                precompute_modified_expr(&arm.body, modified, cache);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            precompute_modified_expr(value, modified, cache);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            precompute_modified_expr(expr, modified, cache);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            precompute_modified_expr(scrutinee, modified, cache);
            for arm in arms {
                modified.extend(precompute_modified_block(arm, cache));
            }
            modified.extend(precompute_modified_block(default, cache));
        }
        // Leaf nodes
        _ => {}
    }
}

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
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn collect_modified_in_expr(expr: &TirExpr, modified: &mut IndexSet<u32>) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            if let TirExprKind::Local { index, .. } = &target.kind {
                modified.insert(*index);
            }
            if let TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } = &target.kind
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
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
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
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
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
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::Cast { expr: inner, .. } => {
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
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Apply store-to-load forwarding to all functions in the project.
pub fn forward_stores_to_loads(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= forward_in_function(&mut func, &type_table);
    }
    changed
}

fn forward_in_function(func: &mut TirFunction, type_table: &TypeTable) -> bool {
    let Some(body) = func.body.as_ref() else {
        return false;
    };

    // Phase 1: Collect unsafe locals (address taken, captured, etc.)
    let unsafe_locals = collect_unsafe_locals(body);

    // Phase 2: Precompute modified locals for all blocks in a single O(n) pass.
    // This replaces the previous O(n²) approach where collect_modified_locals
    // was called inline at every control-flow node during forwarding.
    let cache = precompute_all_modified_locals(body);

    // Phase 3: Forward known values using cached modified-locals lookups
    let body = func.body.as_mut().unwrap();
    let mut known = KnownValues::default();
    forward_in_block(body, &mut known, &unsafe_locals, type_table, &cache)
}

/// Look up precomputed modified locals for a block. Falls back to empty set
/// if the block isn't in the cache (should not happen in practice).
fn lookup_modified(cache: &ModifiedLocalsCache, block: &TirBlock) -> IndexSet<u32> {
    cache
        .get(&std::ptr::from_ref::<TirBlock>(block))
        .cloned()
        .unwrap_or_default()
}

fn forward_in_block(
    block: &mut TirBlock,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    cache: &ModifiedLocalsCache,
) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= forward_in_stmt(stmt, known, unsafe_locals, type_table, cache);
    }
    changed
}

fn forward_in_stmt(
    stmt: &mut TirStmt,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    cache: &ModifiedLocalsCache,
) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            let local_index = *local_index;

            // First, forward loads in the initializer expression
            let changed = forward_in_expr(value, known, unsafe_locals, type_table, cache);

            // Then record known values from this let binding
            if !unsafe_locals.contains(&local_index)
                && let Some(fv) = ForwardValue::from_expr(value)
            {
                known.set_local(local_index, fv);
            }

            changed
        }
        TirStmtKind::Expr(expr) => {
            let changed = forward_in_expr(expr, known, unsafe_locals, type_table, cache);
            // Check for assignments that update known values
            update_known_from_assign(expr, known, unsafe_locals);
            changed
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                forward_in_expr(v, known, unsafe_locals, type_table, cache)
            } else {
                false
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = forward_in_expr(condition, known, unsafe_locals, type_table, cache);
            // Look up precomputed modified locals instead of re-scanning
            let mut modified = lookup_modified(cache, then_block);
            if let Some(eb) = else_block.as_ref() {
                modified.extend(lookup_modified(cache, eb));
            }
            // Forward into branches with cloned state
            let mut then_known = known.clone();
            changed |= forward_in_block(
                then_block,
                &mut then_known,
                unsafe_locals,
                type_table,
                cache,
            );
            if let Some(eb) = else_block {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, unsafe_locals, type_table, cache);
            }
            // Only invalidate locals that were modified in branches
            known.invalidate_modified(&modified);
            changed
        }
        TirStmtKind::Loop { body } => {
            let modified = lookup_modified(cache, body);
            known.invalidate_modified(&modified);
            let mut loop_known = known.clone();
            let changed = forward_in_block(body, &mut loop_known, unsafe_locals, type_table, cache);
            known.invalidate_modified(&modified);
            changed
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            // Labeled blocks can `break` early, so we can't trust the final
            // known state for locals modified inside the block.
            let modified = lookup_modified(cache, block);
            let changed = forward_in_block(block, known, unsafe_locals, type_table, cache);
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
            let mut changed = forward_in_expr(scrutinee, known, unsafe_locals, type_table, cache);
            let mut modified = lookup_modified(cache, then_block);
            if let Some(eb) = else_block.as_ref() {
                modified.extend(lookup_modified(cache, eb));
            }
            let mut then_known = known.clone();
            changed |= forward_in_block(
                then_block,
                &mut then_known,
                unsafe_locals,
                type_table,
                cache,
            );
            if let Some(eb) = else_block {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, unsafe_locals, type_table, cache);
            }
            known.invalidate_modified(&modified);
            changed
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                forward_in_expr(v, known, unsafe_locals, type_table, cache)
            } else {
                false
            }
        }
        TirStmtKind::Continue => false,
        TirStmtKind::LetDestructure { value, .. } => {
            forward_in_expr(value, known, unsafe_locals, type_table, cache)
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

/// Forward known values within an expression tree.
fn forward_in_expr(
    expr: &mut TirExpr,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    cache: &ModifiedLocalsCache,
) -> bool {
    let mut changed = false;

    // Try to forward this expression
    changed |= try_forward_expr(expr, known);

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= forward_in_expr(left, known, unsafe_locals, type_table, cache);
            changed |= forward_in_expr(right, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::Assign { target, value } => {
            changed |= forward_in_expr(value, known, unsafe_locals, type_table, cache);
            // Update known state from this assignment
            update_known_from_target(target, value, known, unsafe_locals);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, unsafe_locals, type_table, cache);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(arg, known, unsafe_locals, type_table, cache);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= forward_in_expr(receiver, known, unsafe_locals, type_table, cache);
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, unsafe_locals, type_table, cache);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            changed |= forward_in_expr(callee, known, unsafe_locals, type_table, cache);
            for arg in args {
                changed |= forward_in_expr(arg, known, unsafe_locals, type_table, cache);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= forward_in_expr(functor, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::Cast { expr: inner, .. } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table, cache);
            changed |= forward_in_expr(index, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::Block(block) => {
            changed |= forward_in_block(block, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            // Labeled blocks in expression position can also break early
            let modified = lookup_modified(cache, block);
            changed |= forward_in_block(block, known, unsafe_locals, type_table, cache);
            known.invalidate_modified(&modified);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= forward_in_expr(condition, known, unsafe_locals, type_table, cache);
            let mut modified = lookup_modified(cache, then_branch);
            if let Some(eb) = else_branch.as_ref() {
                modified.extend(lookup_modified(cache, eb));
            }
            let mut then_known = known.clone();
            changed |= forward_in_block(
                then_branch,
                &mut then_known,
                unsafe_locals,
                type_table,
                cache,
            );
            if let Some(eb) = else_branch {
                let mut else_known = known.clone();
                changed |= forward_in_block(eb, &mut else_known, unsafe_locals, type_table, cache);
            }
            known.invalidate_modified(&modified);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |=
                    forward_in_expr(&mut field.value, known, unsafe_locals, type_table, cache);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                changed |= forward_in_expr(elem, known, unsafe_locals, type_table, cache);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= forward_in_expr(p, known, unsafe_locals, type_table, cache);
            }
        }
        TirExprKind::Closure { .. } => {
            // Don't propagate into closures
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table, cache);
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
                    changed |=
                        forward_in_expr(guard, &mut arm_known, unsafe_locals, type_table, cache);
                }
                changed |= forward_in_expr(
                    &mut arm.body,
                    &mut arm_known,
                    unsafe_locals,
                    type_table,
                    cache,
                );
            }
            known.invalidate_modified(&modified);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= forward_in_expr(value, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            changed |= forward_in_expr(expr, known, unsafe_locals, type_table, cache);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= forward_in_expr(scrutinee, known, unsafe_locals, type_table, cache);
            let mut modified = IndexSet::default();
            for arm in arms.iter() {
                modified.extend(lookup_modified(cache, arm));
            }
            modified.extend(lookup_modified(cache, default));
            for arm in arms {
                let mut arm_known = known.clone();
                changed |= forward_in_block(arm, &mut arm_known, unsafe_locals, type_table, cache);
            }
            let mut default_known = known.clone();
            changed |= forward_in_block(
                default,
                &mut default_known,
                unsafe_locals,
                type_table,
                cache,
            );
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
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
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
