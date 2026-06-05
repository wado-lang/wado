//! Store-to-Load Forwarding optimization for Wado NIR
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
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;

/// Precomputed modified-locals cache, keyed by block raw pointer.
///
/// Built in a single O(n) bottom-up pass before the forward traversal,
/// replacing the previous O(n²) approach where `collect_modified_locals`
/// was called inline at every control-flow node (re-walking subtrees).
type ModifiedLocalsCache = IndexMap<*const NirBlock, IndexSet<u32>>;

/// Precompute modified locals for all blocks in a single bottom-up pass.
fn precompute_all_modified_locals(body: &NirBlock) -> ModifiedLocalsCache {
    let mut cache = ModifiedLocalsCache::default();
    precompute_modified_block(body, &mut cache);
    cache
}

/// Returns the modified set for this block, also inserting it into the cache.
fn precompute_modified_block(block: &NirBlock, cache: &mut ModifiedLocalsCache) -> IndexSet<u32> {
    let mut modified = IndexSet::default();
    for stmt in &block.stmts {
        precompute_modified_stmt(stmt, &mut modified, cache);
    }
    cache.insert(std::ptr::from_ref::<NirBlock>(block), modified.clone());
    modified
}

fn precompute_modified_stmt(
    stmt: &NirStmt,
    modified: &mut IndexSet<u32>,
    cache: &mut ModifiedLocalsCache,
) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => precompute_modified_expr(value, modified, cache),
        NirStmtKind::Expr(expr) => precompute_modified_expr(expr, modified, cache),
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                precompute_modified_expr(v, modified, cache);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => {
            modified.extend(precompute_modified_block(body, cache));
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            modified.extend(precompute_modified_block(block, cache));
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                precompute_modified_expr(v, modified, cache);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => {
            precompute_modified_expr(value, modified, cache);
        }
    }
}

fn precompute_modified_expr(
    expr: &NirExpr,
    modified: &mut IndexSet<u32>,
    cache: &mut ModifiedLocalsCache,
) {
    match &expr.kind {
        NirExprKind::Assign { target, value } => {
            if let NirExprKind::Local { index, .. } = &target.kind {
                modified.insert(*index);
            }
            if let NirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                && let NirExprKind::Local { index, .. } = &inner.kind
            {
                modified.insert(*index);
            }
            precompute_modified_expr(value, modified, cache);
        }
        NirExprKind::Binary { left, right, .. } => {
            precompute_modified_expr(left, modified, cache);
            precompute_modified_expr(right, modified, cache);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. } => {
            precompute_modified_expr(inner, modified, cache);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            precompute_modified_expr(inner, modified, cache);
            precompute_modified_expr(index, modified, cache);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                precompute_modified_expr(&arg.expr, modified, cache);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                precompute_modified_expr(arg, modified, cache);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            precompute_modified_expr(receiver, modified, cache);
            for arg in args {
                precompute_modified_expr(&arg.expr, modified, cache);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            precompute_modified_expr(callee, modified, cache);
            for arg in args {
                precompute_modified_expr(arg, modified, cache);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            precompute_modified_expr(functor, modified, cache);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            modified.extend(precompute_modified_block(block, cache));
        }
        NirExprKind::If {
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
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                precompute_modified_expr(&field.value, modified, cache);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } | NirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                precompute_modified_expr(elem, modified, cache);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                precompute_modified_expr(p, modified, cache);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            precompute_modified_expr(inner, modified, cache);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    precompute_modified_expr(guard, modified, cache);
                }
                precompute_modified_expr(&arm.body, modified, cache);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            precompute_modified_expr(value, modified, cache);
        }
        NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. } => {
            precompute_modified_expr(expr, modified, cache);
        }
        NirExprKind::Switch {
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
    fn from_expr(expr: &NirExpr) -> Option<Self> {
        match &expr.kind {
            NirExprKind::IntLiteral { value, repr } => Some(Self::Int {
                value: *value,
                repr: repr.clone(),
            }),
            NirExprKind::FloatLiteral { value, repr } => Some(Self::Float {
                value: *value,
                repr: repr.clone(),
            }),
            NirExprKind::BoolLiteral(b) => Some(Self::Bool(*b)),
            NirExprKind::CharLiteral(c) => Some(Self::Char(*c)),
            _ => None,
        }
    }

    fn to_expr_kind(&self) -> NirExprKind {
        match self {
            Self::Int { value, repr } => NirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Float { value, repr } => NirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Bool(b) => NirExprKind::BoolLiteral(*b),
            Self::Char(c) => NirExprKind::CharLiteral(*c),
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
fn collect_modified_locals(block: &NirBlock) -> IndexSet<u32> {
    let mut modified = IndexSet::default();
    for stmt in &block.stmts {
        collect_modified_in_stmt(stmt, &mut modified);
    }
    modified
}

fn collect_modified_in_stmt(stmt: &NirStmt, modified: &mut IndexSet<u32>) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => collect_modified_in_expr(value, modified),
        NirStmtKind::Expr(expr) => collect_modified_in_expr(expr, modified),
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_in_expr(v, modified);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => {
            modified.extend(collect_modified_locals(body));
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            modified.extend(collect_modified_locals(block));
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_modified_in_expr(v, modified);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => collect_modified_in_expr(value, modified),
    }
}

fn collect_modified_in_expr(expr: &NirExpr, modified: &mut IndexSet<u32>) {
    match &expr.kind {
        NirExprKind::Assign { target, value } => {
            if let NirExprKind::Local { index, .. } = &target.kind {
                modified.insert(*index);
            }
            if let NirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                && let NirExprKind::Local { index, .. } = &inner.kind
            {
                modified.insert(*index);
            }
            collect_modified_in_expr(value, modified);
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_modified_in_expr(left, modified);
            collect_modified_in_expr(right, modified);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. } => {
            collect_modified_in_expr(inner, modified);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_modified_in_expr(inner, modified);
            collect_modified_in_expr(index, modified);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_modified_in_expr(&arg.expr, modified);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_modified_in_expr(arg, modified);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_modified_in_expr(receiver, modified);
            for arg in args {
                collect_modified_in_expr(&arg.expr, modified);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            collect_modified_in_expr(callee, modified);
            for arg in args {
                collect_modified_in_expr(arg, modified);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_modified_in_expr(functor, modified);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            modified.extend(collect_modified_locals(block));
        }
        NirExprKind::If {
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
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_modified_in_expr(&field.value, modified);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } | NirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                collect_modified_in_expr(elem, modified);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_modified_in_expr(p, modified);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            collect_modified_in_expr(inner, modified);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_modified_in_expr(guard, modified);
                }
                collect_modified_in_expr(&arm.body, modified);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            collect_modified_in_expr(value, modified);
        }
        NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. } => {
            collect_modified_in_expr(expr, modified);
        }
        NirExprKind::Switch {
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
fn collect_unsafe_locals(body: &NirBlock) -> IndexSet<u32> {
    let mut unsafe_locals = IndexSet::default();
    collect_unsafe_in_block(body, &mut unsafe_locals);
    unsafe_locals
}

fn collect_unsafe_in_block(block: &NirBlock, unsafe_locals: &mut IndexSet<u32>) {
    for stmt in &block.stmts {
        collect_unsafe_in_stmt(stmt, unsafe_locals);
    }
}

fn collect_unsafe_in_stmt(stmt: &NirStmt, unsafe_locals: &mut IndexSet<u32>) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => collect_unsafe_in_expr(value, unsafe_locals),
        NirStmtKind::Expr(expr) => collect_unsafe_in_expr(expr, unsafe_locals),
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_unsafe_in_expr(v, unsafe_locals);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => collect_unsafe_in_block(body, unsafe_locals),
        NirStmtKind::LabeledBlock { block, .. } => {
            collect_unsafe_in_block(block, unsafe_locals);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_unsafe_in_expr(v, unsafe_locals);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => collect_unsafe_in_expr(value, unsafe_locals),
    }
}

fn collect_unsafe_in_expr(expr: &NirExpr, unsafe_locals: &mut IndexSet<u32>) {
    match &expr.kind {
        NirExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => {
            // Address taken: mark the local as unsafe
            if let NirExprKind::Local { index, .. } = &inner.kind {
                unsafe_locals.insert(*index);
            }
            collect_unsafe_in_expr(inner, unsafe_locals);
        }
        NirExprKind::Unary { expr: inner, .. } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_unsafe_in_expr(left, unsafe_locals);
            collect_unsafe_in_expr(right, unsafe_locals);
        }
        NirExprKind::Assign { target, value } => {
            collect_unsafe_in_expr(target, unsafe_locals);
            collect_unsafe_in_expr(value, unsafe_locals);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_unsafe_in_expr(&arg.expr, unsafe_locals);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_unsafe_in_expr(arg, unsafe_locals);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_unsafe_in_expr(receiver, unsafe_locals);
            for arg in args {
                collect_unsafe_in_expr(&arg.expr, unsafe_locals);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            collect_unsafe_in_expr(callee, unsafe_locals);
            for arg in args {
                collect_unsafe_in_expr(arg, unsafe_locals);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_unsafe_in_expr(functor, unsafe_locals);
        }
        NirExprKind::FieldAccess { expr: inner, .. } | NirExprKind::Cast { expr: inner, .. } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
            collect_unsafe_in_expr(index, unsafe_locals);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            collect_unsafe_in_block(block, unsafe_locals);
        }
        NirExprKind::If {
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
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_unsafe_in_expr(&field.value, unsafe_locals);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } | NirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                collect_unsafe_in_expr(elem, unsafe_locals);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_unsafe_in_expr(p, unsafe_locals);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            collect_unsafe_in_expr(inner, unsafe_locals);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_unsafe_in_expr(guard, unsafe_locals);
                }
                collect_unsafe_in_expr(&arm.body, unsafe_locals);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            collect_unsafe_in_expr(value, unsafe_locals);
        }
        NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. } => {
            collect_unsafe_in_expr(expr, unsafe_locals);
        }
        NirExprKind::Switch {
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
        NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Apply store-to-load forwarding to all functions in the project.
pub fn forward_stores_to_loads(project: &mut NirPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= forward_in_function(&mut func, &type_table);
    }
    changed
}

fn forward_in_function(func: &mut NirFunction, type_table: &TypeTable) -> bool {
    let Some(mut owned) = func.body_block() else {
        return false;
    };

    // Phase 1: Collect unsafe locals (address taken, captured, etc.)
    let unsafe_locals = collect_unsafe_locals(&owned);

    // Phase 2: Precompute modified locals for all blocks in a single O(n) pass.
    // This replaces the previous O(n²) approach where collect_modified_locals
    // was called inline at every control-flow node during forwarding.
    let cache = precompute_all_modified_locals(&owned);

    // Phase 3: Forward known values using cached modified-locals lookups
    let body = &mut owned;
    let mut known = KnownValues::default();
    let r = forward_in_block(body, &mut known, &unsafe_locals, type_table, &cache);
    func.set_body_block(owned);
    r
}

/// Look up precomputed modified locals for a block. Falls back to empty set
/// if the block isn't in the cache (should not happen in practice).
fn lookup_modified(cache: &ModifiedLocalsCache, block: &NirBlock) -> IndexSet<u32> {
    cache
        .get(&std::ptr::from_ref::<NirBlock>(block))
        .cloned()
        .unwrap_or_default()
}

fn forward_in_block(
    block: &mut NirBlock,
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
    stmt: &mut NirStmt,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    cache: &ModifiedLocalsCache,
) -> bool {
    match &mut stmt.kind {
        NirStmtKind::Let {
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
        NirStmtKind::Expr(expr) => {
            let changed = forward_in_expr(expr, known, unsafe_locals, type_table, cache);
            // Check for assignments that update known values
            update_known_from_assign(expr, known, unsafe_locals);
            changed
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                forward_in_expr(v, known, unsafe_locals, type_table, cache)
            } else {
                false
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => {
            let modified = lookup_modified(cache, body);
            known.invalidate_modified(&modified);
            let mut loop_known = known.clone();
            let changed = forward_in_block(body, &mut loop_known, unsafe_locals, type_table, cache);
            known.invalidate_modified(&modified);
            changed
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            // Labeled blocks can `break` early, so we can't trust the final
            // known state for locals modified inside the block.
            let modified = lookup_modified(cache, block);
            let changed = forward_in_block(block, known, unsafe_locals, type_table, cache);
            // Invalidate only locals that were modified in the block
            known.invalidate_modified(&modified);
            changed
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                forward_in_expr(v, known, unsafe_locals, type_table, cache)
            } else {
                false
            }
        }
        NirStmtKind::Continue => false,
        NirStmtKind::LetDestructure { value, .. } => {
            forward_in_expr(value, known, unsafe_locals, type_table, cache)
        }
    }
}

/// Forward known values within an expression tree.
fn forward_in_expr(
    expr: &mut NirExpr,
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
        NirExprKind::Binary { left, right, .. } => {
            changed |= forward_in_expr(left, known, unsafe_locals, type_table, cache);
            changed |= forward_in_expr(right, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::Unary { expr: inner, .. } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::Assign { target, value } => {
            changed |= forward_in_expr(value, known, unsafe_locals, type_table, cache);
            // Update known state from this assignment
            update_known_from_target(target, value, known, unsafe_locals);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, unsafe_locals, type_table, cache);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= forward_in_expr(arg, known, unsafe_locals, type_table, cache);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            changed |= forward_in_expr(receiver, known, unsafe_locals, type_table, cache);
            for arg in args {
                changed |= forward_in_expr(&mut arg.expr, known, unsafe_locals, type_table, cache);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            changed |= forward_in_expr(callee, known, unsafe_locals, type_table, cache);
            for arg in args {
                changed |= forward_in_expr(arg, known, unsafe_locals, type_table, cache);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= forward_in_expr(functor, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::FieldAccess { expr: inner, .. } | NirExprKind::Cast { expr: inner, .. } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            changed |= forward_in_expr(inner, known, unsafe_locals, type_table, cache);
            changed |= forward_in_expr(index, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::Block(block) => {
            changed |= forward_in_block(block, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::LabeledBlock { block, .. } => {
            // Labeled blocks in expression position can also break early
            let modified = lookup_modified(cache, block);
            changed |= forward_in_block(block, known, unsafe_locals, type_table, cache);
            known.invalidate_modified(&modified);
        }
        NirExprKind::If {
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
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |=
                    forward_in_expr(&mut field.value, known, unsafe_locals, type_table, cache);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } | NirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                changed |= forward_in_expr(elem, known, unsafe_locals, type_table, cache);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= forward_in_expr(p, known, unsafe_locals, type_table, cache);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
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
        NirExprKind::GlobalVarSet { value, .. } => {
            changed |= forward_in_expr(value, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. } => {
            changed |= forward_in_expr(expr, known, unsafe_locals, type_table, cache);
        }
        NirExprKind::Switch {
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
        NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => {}
    }

    changed
}

/// Try to substitute a local read with a known literal.
fn try_forward_expr(expr: &mut NirExpr, known: &KnownValues) -> bool {
    if let NirExprKind::Local { index, .. } = &expr.kind
        && let Some(fv) = known.get_local(*index)
    {
        expr.kind = fv.to_expr_kind();
        return true;
    }
    false
}

/// Update known values from an assignment target.
fn update_known_from_target(
    target: &NirExpr,
    value: &NirExpr,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
) {
    if let NirExprKind::Local { index, .. } = &target.kind {
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
    expr: &NirExpr,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
) {
    if let NirExprKind::Assign { target, value } = &expr.kind {
        update_known_from_target(target, value, known, unsafe_locals);
    }
}
