//! Template String Buffer Hoisting for Loops
//!
//! When a template string (`__tmpl` labeled block) appears inside a loop,
//! this pass hoists the entire `String` allocation before the loop and reuses
//! it across iterations, resetting `used = 0` instead of creating a new struct.
//!
//! **Before:**
//! ```text
//! loop {
//!     let s = __tmpl: {
//!         let mut __r = String { repr: array_new(N), used: 0 };
//!         __r.push_str(...);
//!         break __tmpl: __r;
//!     };
//!     s.len();   // s only used as method receiver
//! }
//! ```
//!
//! **After:**
//! ```text
//! let mut __tmpl_buf_0 = String { repr: array_new(N), used: 0 };
//! loop {
//!     let s /* skip_value_copy */ = __tmpl: {
//!         __tmpl_buf_0.used = 0;        // reset (no struct.new)
//!         __tmpl_buf_0.push_str(...);      // reuse same String
//!         break __tmpl: __tmpl_buf_0;
//!     };
//!     s.len();   // s aliases __tmpl_buf_0
//! }
//! ```
//!
//! Safety: The optimization reuses the same String GC struct across iterations.
//! It is only applied when the template result does not escape the iteration:
//! the result must be bound to a Let variable that is only used as a method
//! receiver (`self`), never passed as a regular function argument.
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and walks every loop in the function, applying the template-
//! string buffer hoist per loop. All mutations route through the engine edit
//! API (`alloc_expr`, `alloc_stmt`, `alloc_local`, `set_block_stmts`,
//! `replace_expr_kind`) so the parent map and use index stay coherent.

use std::cell::{Cell, RefCell};

use crate::compiler_item::SeqField;
use crate::hashmap::IndexSet;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{
    ArenaStructField, BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::arena_query::is_local;
use super::gate::{FunctionGate, GatedPass};
use cranelift_entity::EntityRef;

/// Apply template string buffer hoisting to all functions in the project.
pub fn hoist_template_buffers(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.clone();
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::TmplHoist, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = TmplHoistRule {
            type_table: &type_table,
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function template-buffer hoist at the body root.
pub(super) struct TmplHoistRule<'a> {
    type_table: &'a RefCell<TypeTable>,
    applied: Cell<bool>,
}

impl Rule for TmplHoistRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        let root = engine.body.root;
        hoist_in_block(engine, root, self.type_table)
    }
}

fn hoist_in_block(engine: &mut Engine, block: BlockId, type_table: &RefCell<TypeTable>) -> bool {
    let mut changed = false;
    let mut new_stmts: Vec<StmtId> = Vec::new();

    for s in engine.body.blocks[block].stmts.clone() {
        // Classify without holding the borrow across the mutable recursion.
        let (loop_b, if_blocks, lab_b) = match &engine.body.stmts[s].kind {
            StmtKind::Loop { body } => (Some(*body), None, None),
            StmtKind::If {
                then_block,
                else_block,
                ..
            } => (None, Some((*then_block, *else_block)), None),
            StmtKind::LabeledBlock { block, .. } => (None, None, Some(*block)),
            _ => (None, None, None),
        };

        if let Some(lb) = loop_b {
            // Recurse into loop body first (for nested loops).
            changed |= hoist_in_block(engine, lb, type_table);
            // Try to hoist template buffers out of this loop.
            let hoist_stmts = hoist_tmpl_from_loop(engine, lb, type_table);
            if !hoist_stmts.is_empty() {
                changed = true;
                new_stmts.extend(hoist_stmts);
            }
            new_stmts.push(s);
        } else if let Some((tb, eb)) = if_blocks {
            changed |= hoist_in_block(engine, tb, type_table);
            if let Some(eb) = eb {
                changed |= hoist_in_block(engine, eb, type_table);
            }
            new_stmts.push(s);
        } else if let Some(inner) = lab_b {
            changed |= hoist_in_block(engine, inner, type_table);
            new_stmts.push(s);
        } else {
            new_stmts.push(s);
        }
    }

    engine.set_block_stmts(block, new_stmts);
    changed
}

/// Information about a `__tmpl` block that can be hoisted.
struct TmplCandidate {
    /// Index of the `__r` local in the `__tmpl` block
    buf_local_index: u32,
    /// The init-value expression node id (e.g. `String { repr: array_new(N),
    /// used: 0 }`). Reused as the hoisted `Let`'s value — the original first
    /// statement is replaced, so this subtree becomes the hoist's sole owner.
    init_value: ExprId,
    /// The String type ID
    string_type: TypeId,
    /// The span of the original expression
    span: Span,
}

/// Information about a Formatter struct literal that can be hoisted out of a `__tmpl` block.
struct FmtCandidate {
    /// Index of the statement inside the `__tmpl` block that creates the Formatter
    stmt_index: usize,
    /// The local index being assigned to (e.g., `__local_13`)
    fmt_local_index: u32,
    /// The normalized Formatter struct-literal node id (`buf` pointing at the
    /// hoisted buffer). Reused as the hoisted `Let`'s value.
    init_value: ExprId,
    /// The Formatter type ID
    formatter_type: TypeId,
    /// Index of the `indent` field in the Formatter struct
    indent_field_index: u32,
    /// The span
    span: Span,
}

/// Scan a loop body for `__tmpl` labeled blocks and hoist their buffer allocations.
/// Returns hoisting statements to prepend before the loop.
fn hoist_tmpl_from_loop(
    engine: &mut Engine,
    loop_body: BlockId,
    type_table: &RefCell<TypeTable>,
) -> Vec<StmtId> {
    // Phase 1: Collect all Let bindings whose value is a __tmpl LabeledBlock,
    // and check if the bound variable escapes (used as a non-self argument).
    let escaping_locals = collect_escaping_locals(engine.body, loop_body);

    // Phase 2: Transform safe __tmpl blocks
    let mut hoist_stmts = Vec::new();
    transform_stmts_in_block(
        engine,
        loop_body,
        &escaping_locals,
        &mut hoist_stmts,
        type_table,
    );
    hoist_stmts
}

/// Collect local indices that "escape" — used as a non-receiver function argument
/// or stored in a struct/array/collection.
fn collect_escaping_locals(body: &Body, block: BlockId) -> IndexSet<u32> {
    let mut escaping = IndexSet::default();
    for s in &body.blocks[block].stmts {
        collect_escaping_in_stmt(body, *s, &mut escaping);
    }
    escaping
}

fn collect_escaping_in_stmt(body: &Body, s: StmtId, escaping: &mut IndexSet<u32>) {
    match &body.stmts[s].kind {
        StmtKind::Let { value, .. } => {
            collect_escaping_in_expr(body, *value, escaping);
        }
        StmtKind::Expr(expr) => {
            collect_escaping_in_expr(body, *expr, escaping);
        }
        StmtKind::Return { value: Some(expr) } => {
            // Returning a value means it escapes the loop
            collect_local_refs(body, *expr, escaping);
            collect_escaping_in_expr(body, *expr, escaping);
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let condition = *condition;
            let then_block = *then_block;
            let else_block = *else_block;
            collect_escaping_in_expr(body, condition, escaping);
            for s in &body.blocks[then_block].stmts {
                collect_escaping_in_stmt(body, *s, escaping);
            }
            if let Some(eb) = else_block {
                for s in &body.blocks[eb].stmts {
                    collect_escaping_in_stmt(body, *s, escaping);
                }
            }
        }
        StmtKind::LabeledBlock { block, .. } => {
            let block = *block;
            for s in &body.blocks[block].stmts {
                collect_escaping_in_stmt(body, *s, escaping);
            }
        }
        StmtKind::Loop { body: lb } => {
            let lb = *lb;
            for s in &body.blocks[lb].stmts {
                collect_escaping_in_stmt(body, *s, escaping);
            }
        }
        StmtKind::Return { value: None } | StmtKind::Continue => {}
        StmtKind::Break { value, .. } => {
            if let Some(v) = value {
                let v = *v;
                collect_local_refs(body, v, escaping);
                collect_escaping_in_expr(body, v, escaping);
            }
        }
        StmtKind::LetDestructure { value, .. } => {
            collect_escaping_in_expr(body, *value, escaping);
        }
    }
}

/// Mark all locals that appear as non-receiver function arguments as escaping.
fn collect_escaping_in_expr(body: &Body, e: ExprId, escaping: &mut IndexSet<u32>) {
    match &body.exprs[e].kind {
        // Function call: args (not receiver) escape
        ExprKind::Call { args, .. } => {
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for arg in args {
                collect_local_refs(body, arg, escaping);
                collect_escaping_in_expr(body, arg, escaping);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            // Receiver (self) doesn't escape — only non-self args escape
            let receiver = *receiver;
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            collect_escaping_in_expr(body, receiver, escaping);
            for arg in args {
                collect_local_refs(body, arg, escaping);
                collect_escaping_in_expr(body, arg, escaping);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let args = args.clone();
            collect_escaping_in_expr(body, callee, escaping);
            for arg in args {
                collect_local_refs(body, arg, escaping);
                collect_escaping_in_expr(body, arg, escaping);
            }
        }
        // Assignment: the value escapes (stored in target location)
        ExprKind::Assign { target, value } => {
            let target = *target;
            let value = *value;
            collect_local_refs(body, value, escaping);
            collect_escaping_in_expr(body, target, escaping);
            collect_escaping_in_expr(body, value, escaping);
        }
        // Struct literal fields: all field values escape
        ExprKind::StructLiteral { fields, .. } => {
            let vals: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            for v in vals {
                collect_local_refs(body, v, escaping);
                collect_escaping_in_expr(body, v, escaping);
            }
        }
        // Index expressions: the index value may escape (used in indexing)
        ExprKind::Index { expr: inner, index } => {
            let inner = *inner;
            let index = *index;
            collect_escaping_in_expr(body, inner, escaping);
            collect_escaping_in_expr(body, index, escaping);
        }
        // Binary/unary: recurse
        ExprKind::Binary { left, right, .. } => {
            let left = *left;
            let right = *right;
            collect_escaping_in_expr(body, left, escaping);
            collect_escaping_in_expr(body, right, escaping);
        }
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            collect_escaping_in_expr(body, *inner, escaping);
        }
        ExprKind::FieldAccess { expr: inner, .. } => {
            collect_escaping_in_expr(body, *inner, escaping);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let condition = *condition;
            let then_branch = *then_branch;
            let else_branch = *else_branch;
            collect_escaping_in_expr(body, condition, escaping);
            for s in &body.blocks[then_branch].stmts {
                collect_escaping_in_stmt(body, *s, escaping);
            }
            if let Some(eb) = else_branch {
                for s in &body.blocks[eb].stmts {
                    collect_escaping_in_stmt(body, *s, escaping);
                }
            }
        }
        ExprKind::LabeledBlock { block, .. } => {
            let block = *block;
            for s in &body.blocks[block].stmts {
                collect_escaping_in_stmt(body, *s, escaping);
            }
        }
        ExprKind::Block(block) => {
            let block = *block;
            for s in &body.blocks[block].stmts {
                collect_escaping_in_stmt(body, *s, escaping);
            }
        }
        ExprKind::Match { expr: inner, arms } => {
            let inner = *inner;
            let arm_exprs: Vec<ExprId> = arms
                .iter()
                .flat_map(|arm| arm.guard.into_iter().chain(std::iter::once(arm.body)))
                .collect();
            collect_escaping_in_expr(body, inner, escaping);
            for ae in arm_exprs {
                collect_escaping_in_expr(body, ae, escaping);
            }
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            collect_escaping_in_expr(body, scrutinee, escaping);
            for arm in arms {
                for s in &body.blocks[arm].stmts {
                    collect_escaping_in_stmt(body, *s, escaping);
                }
            }
            for s in &body.blocks[default].stmts {
                collect_escaping_in_stmt(body, *s, escaping);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let args = args.clone();
            for arg in args {
                collect_local_refs(body, arg, escaping);
                collect_escaping_in_expr(body, arg, escaping);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            for elem in elements {
                collect_local_refs(body, elem, escaping);
                collect_escaping_in_expr(body, elem, escaping);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                let p = *p;
                collect_local_refs(body, p, escaping);
                collect_escaping_in_expr(body, p, escaping);
            }
        }
        ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. } => {
            collect_escaping_in_expr(body, *inner, escaping);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            let value = *value;
            collect_local_refs(body, value, escaping);
            collect_escaping_in_expr(body, value, escaping);
        }
        // Leaf nodes
        ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::StringLiteral(_)
        | ExprKind::BytesLiteral(_)
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::Null
        | ExprKind::Unit
        | ExprKind::EnumConstruct { .. } => {}
    }
}

/// Collect all local variable indices that directly appear in an expression.
/// Does NOT follow through `FieldAccess`: `s.repr` as a struct field value does
/// not mark `s` as escaping, since field extraction is typically for temporary
/// iterators/formatters consumed within the same scope.
fn collect_local_refs(body: &Body, e: ExprId, locals: &mut IndexSet<u32>) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            locals.insert(*index);
        }
        ExprKind::LabeledBlock { block, .. } => {
            // The block result is the break value — check those
            let block = *block;
            let break_vals: Vec<ExprId> = body.blocks[block]
                .stmts
                .iter()
                .filter_map(|s| match &body.stmts[*s].kind {
                    StmtKind::Break {
                        value: Some(val), ..
                    } => Some(*val),
                    _ => None,
                })
                .collect();
            for val in break_vals {
                collect_local_refs(body, val, locals);
            }
        }
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            collect_local_refs(body, *inner, locals);
        }
        // FieldAccess (e.g., s.repr) — accessing a subfield doesn't mean the whole
        // local escapes. Skip to avoid false positives with iterator construction.
        _ => {}
    }
}

/// Recursively transform statements, looking for Let bindings with __tmpl blocks.
fn transform_stmts_in_block(
    engine: &mut Engine,
    block: BlockId,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
) {
    for s in engine.body.blocks[block].stmts.clone() {
        transform_stmt(engine, s, escaping_locals, hoist_stmts, type_table);
    }
}

fn transform_stmt(
    engine: &mut Engine,
    s: StmtId,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
) {
    // `let x = __tmpl: { ... }` — the only statement shape that can hoist.
    let let_info = if let StmtKind::Let {
        local_index, value, ..
    } = &engine.body.stmts[s].kind
    {
        Some((*local_index, *value))
    } else {
        None
    };
    if let Some((local_index, value)) = let_info {
        let tmpl_block = match &engine.body.exprs[value].kind {
            ExprKind::LabeledBlock { label, block, .. } if label == "__tmpl" => Some(*block),
            _ => None,
        };
        if let Some(tb) = tmpl_block
            && !escaping_locals.contains(&local_index)
            && let Some(candidate) = extract_tmpl_candidate(engine.body, tb)
        {
            transform_tmpl_block(engine, tb, &candidate, hoist_stmts, type_table);
            // The hoisted String is reused; skip deep copy so `s` aliases `__tmpl_buf`.
            // This is a non-id field on `Let` and does not affect the engine's
            // parent map / use index, so the in-place write is safe.
            if let StmtKind::Let {
                skip_value_copy, ..
            } = &mut engine.body.stmts[s].kind
            {
                *skip_value_copy = true;
            }
            return;
        }
        // Recurse into the value expression
        transform_expr(engine, value, escaping_locals, hoist_stmts, type_table);
        return;
    }

    // Other statement shapes that carry transformable expressions / blocks.
    enum Shape {
        Expr(ExprId),
        If(ExprId, BlockId, Option<BlockId>),
        Labeled(BlockId),
        Break(ExprId),
        None,
    }
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::Expr(e) => Shape::Expr(*e),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => Shape::If(*condition, *then_block, *else_block),
        StmtKind::LabeledBlock { block, .. } => Shape::Labeled(*block),
        StmtKind::Break { value: Some(e), .. } => Shape::Break(*e),
        // Don't recurse into nested loops.
        _ => Shape::None,
    };
    match shape {
        Shape::Expr(e) | Shape::Break(e) => {
            transform_expr(engine, e, escaping_locals, hoist_stmts, type_table)
        }
        Shape::If(cond, tb, eb) => {
            transform_expr(engine, cond, escaping_locals, hoist_stmts, type_table);
            transform_stmts_in_block(engine, tb, escaping_locals, hoist_stmts, type_table);
            if let Some(eb) = eb {
                transform_stmts_in_block(engine, eb, escaping_locals, hoist_stmts, type_table);
            }
        }
        Shape::Labeled(b) => {
            transform_stmts_in_block(engine, b, escaping_locals, hoist_stmts, type_table)
        }
        Shape::None => {}
    }
}

fn transform_expr(
    engine: &mut Engine,
    e: ExprId,
    escaping_locals: &IndexSet<u32>,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
) {
    // Mirror the original's restricted arm set: __tmpl in non-Let contexts is
    // not hoisted, so only these shapes recurse.
    enum Walk {
        Exprs(Vec<ExprId>),
        CondBlocks(ExprId, BlockId, Option<BlockId>),
        Block(BlockId),
        None,
    }
    let walk = match &engine.body.exprs[e].kind {
        ExprKind::Call { args, .. } => Walk::Exprs(args.iter().map(|a| a.expr).collect()),
        ExprKind::MethodCall { receiver, args, .. } => {
            let mut v = vec![*receiver];
            v.extend(args.iter().map(|a| a.expr));
            Walk::Exprs(v)
        }
        ExprKind::Binary { left, right, .. } => Walk::Exprs(vec![*left, *right]),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. } => Walk::Exprs(vec![*inner]),
        ExprKind::Assign { target, value } => Walk::Exprs(vec![*target, *value]),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => Walk::CondBlocks(*condition, *then_branch, *else_branch),
        ExprKind::LabeledBlock { block, .. } | ExprKind::Block(block) => Walk::Block(*block),
        _ => Walk::None,
    };
    match walk {
        Walk::Exprs(v) => {
            for id in v {
                transform_expr(engine, id, escaping_locals, hoist_stmts, type_table);
            }
        }
        Walk::CondBlocks(cond, tb, eb) => {
            transform_expr(engine, cond, escaping_locals, hoist_stmts, type_table);
            transform_stmts_in_block(engine, tb, escaping_locals, hoist_stmts, type_table);
            if let Some(eb) = eb {
                transform_stmts_in_block(engine, eb, escaping_locals, hoist_stmts, type_table);
            }
        }
        Walk::Block(b) => {
            transform_stmts_in_block(engine, b, escaping_locals, hoist_stmts, type_table)
        }
        Walk::None => {}
    }
}

/// Check if a `__tmpl` block has the expected pattern.
///
/// Before lowering:
///   `let mut __r = String::with_capacity(N);`
///
/// After lowering (inlined):
///   `let mut __r = String { repr: array_new<u8>(N), used: 0 };`
///
/// Both end with:
///   `break __tmpl: __r;`
fn extract_tmpl_candidate(body: &Body, block: BlockId) -> Option<TmplCandidate> {
    // First statement must be: let mut __r = ...
    let first_stmt = *body.blocks[block].stmts.first()?;
    let (buf_local_index, string_type, init_value, span) = match &body.stmts[first_stmt].kind {
        StmtKind::Let {
            name,
            local_index,
            value,
            type_id,
            ..
        } if name == "__r" => {
            let local_index = *local_index;
            let value = *value;
            let type_id = *type_id;
            let value_span = body.exprs[value].span;
            // Try pre-lowered form: String::with_capacity(N)
            if let ExprKind::Call { func, .. } = &body.exprs[value].kind
                && func.method_info.is_some()
                && func.name == "String::with_capacity"
            {
                return Some(TmplCandidate {
                    buf_local_index: local_index,
                    init_value: value,
                    string_type: type_id,
                    span: value_span,
                });
            }
            // Try post-lowered form: String { repr: array_new<u8>(N), used: 0 }
            if let ExprKind::StructLiteral {
                struct_name,
                fields,
                ..
            } = &body.exprs[value].kind
            {
                if struct_name == "String" {
                    // Verify the repr field contains an array_new call
                    let repr_field = fields
                        .iter()
                        .find(|f| f.name == SeqField::Backing.field_name())?;
                    if !array_new_has_capacity(body, repr_field.value) {
                        return None;
                    }
                    // Verify used field is 0
                    let used_field = fields
                        .iter()
                        .find(|f| f.name == SeqField::Len.field_name())?;
                    if !matches!(
                        &body.exprs[used_field.value].kind,
                        ExprKind::IntLiteral { value: 0, .. }
                    ) {
                        return None;
                    }
                    (local_index, type_id, value, value_span)
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // Last statement must be: break __tmpl: __r
    let last_stmt = *body.blocks[block].stmts.last()?;
    match &body.stmts[last_stmt].kind {
        StmtKind::Break {
            label: Some(label),
            value: Some(val),
        } if label == "__tmpl" => match &body.exprs[*val].kind {
            ExprKind::Local { index, .. } if *index == buf_local_index => {}
            _ => return None,
        },
        _ => return None,
    }

    Some(TmplCandidate {
        buf_local_index,
        init_value,
        string_type,
        span,
    })
}

/// Owned descriptor of a Formatter struct literal's fields, extracted from the
/// arena so the candidacy decision needs no borrow held across construction.
struct FmtFields {
    struct_name: String,
    fields: Vec<(String, u32, ExprId)>,
    struct_type: TypeId,
    value_type_id: TypeId,
    value_span: Span,
}

/// Collect all Formatter struct literals in a `__tmpl` block that can be hoisted.
///
/// Detects three patterns:
///   1. Assign: `__local_N = Formatter { ... }`
///   2. Let:    `let mut __f = Formatter { ... }`
///   3. `LabeledBlock` (inlined `Formatter::new)`:
///      `let __f = label: { let buf = &mut __tmpl_buf; break: Formatter { ..., buf } }`
///      or `__local_N = label: { ... break: Formatter { ... } }`
fn extract_fmt_candidates(
    engine: &mut Engine,
    block: BlockId,
    hoisted_buf_index: u32,
    type_table: &RefCell<TypeTable>,
) -> Vec<FmtCandidate> {
    // Phase A (read): decide candidacy without mutating the arena.
    struct Raw {
        stmt_index: usize,
        fmt_local_index: u32,
        ff: FmtFields,
        indent_field_index: u32,
    }
    let mut raws: Vec<Raw> = Vec::new();
    {
        let type_table = type_table.borrow();
        let stmts = engine.body.blocks[block].stmts.clone();
        let len = stmts.len();
        for (i, s) in stmts.iter().enumerate() {
            if i == 0 || i == len - 1 {
                continue;
            }

            // Try to extract (local_index, value_expr_id) from the statement.
            let (fmt_local_index, value_expr): (u32, ExprId) = match &engine.body.stmts[*s].kind {
                StmtKind::Expr(expr) => {
                    let ExprKind::Assign { target, value } = &engine.body.exprs[*expr].kind else {
                        continue;
                    };
                    let ExprKind::Local { index, .. } = &engine.body.exprs[*target].kind else {
                        continue;
                    };
                    (*index, *value)
                }
                StmtKind::Let {
                    local_index, value, ..
                } => (*local_index, *value),
                _ => continue,
            };

            let Some(ff) = extract_formatter_fields(engine.body, value_expr, hoisted_buf_index)
            else {
                continue;
            };

            if ff.struct_name != "Formatter" {
                continue;
            }

            // Find the `indent` field index
            let Some((_, indent_field_index, _)) =
                ff.fields.iter().find(|(name, _, _)| name == "indent")
            else {
                continue;
            };
            let indent_field_index = *indent_field_index;

            // Verify all non-buf fields are constant (literals)
            let all_const = ff.fields.iter().all(|(name, _, value)| {
                if name == "buf" {
                    return true;
                }
                is_constant_expr(engine.body, *value)
            });
            if !all_const {
                continue;
            }

            // Verify the Formatter type is a struct
            let resolved = type_table.get(ff.value_type_id);
            if !matches!(resolved, crate::tir::ResolvedType::Struct { .. }) {
                continue;
            }

            raws.push(Raw {
                stmt_index: i,
                fmt_local_index,
                ff,
                indent_field_index,
            });
        }
    }

    // Phase B (build): synthesize the normalized Formatter literal per candidate.
    let mut candidates = Vec::new();
    for raw in raws {
        let value_type_id = raw.ff.value_type_id;
        let value_span = raw.ff.value_span;
        let struct_type = raw.ff.struct_type;
        let mut init_fields: Vec<ArenaStructField> = Vec::new();
        for (name, field_index, value) in &raw.ff.fields {
            let new_value = if name == "buf" {
                // Normalize buf to &mut __tmpl_buf
                let buf_ty = engine.body.exprs[*value].type_id;
                let local = engine.alloc_expr(
                    ExprKind::Local {
                        index: hoisted_buf_index,
                        name: format!("__tmpl_buf_{hoisted_buf_index}"),
                    },
                    buf_ty,
                    value_span,
                );
                engine.alloc_expr(
                    ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr: local,
                    },
                    buf_ty,
                    value_span,
                )
            } else {
                // A verified constant leaf — a shallow node copy is a full copy.
                let node = engine.body.exprs[*value].clone();
                engine.alloc_expr(node.kind, node.type_id, node.span)
            };
            init_fields.push(ArenaStructField {
                name: name.clone(),
                value: new_value,
                field_index: *field_index,
            });
        }
        let init_value = engine.alloc_expr(
            ExprKind::StructLiteral {
                struct_type,
                struct_name: raw.ff.struct_name.clone(),
                fields: init_fields,
            },
            value_type_id,
            value_span,
        );
        candidates.push(FmtCandidate {
            stmt_index: raw.stmt_index,
            fmt_local_index: raw.fmt_local_index,
            init_value,
            formatter_type: struct_type,
            indent_field_index: raw.indent_field_index,
            span: value_span,
        });
    }
    candidates
}

/// Extract Formatter struct literal fields from a value expression.
///
/// Handles:
///   - Direct `StructLiteral { ... }` where buf references hoisted buffer
///   - `LabeledBlock { let buf = ...; break: StructLiteral { ..., buf } }`
///     where the intermediate `buf` local traces back to the hoisted buffer
fn extract_formatter_fields(
    body: &Body,
    value: ExprId,
    hoisted_buf_index: u32,
) -> Option<FmtFields> {
    let value_type_id = body.exprs[value].type_id;
    let value_span = body.exprs[value].span;
    match &body.exprs[value].kind {
        ExprKind::StructLiteral {
            struct_name,
            fields,
            struct_type,
        } => {
            let buf_field = fields.iter().find(|f| f.name == "buf")?;
            if !buf_field_references_local(body, buf_field.value, hoisted_buf_index) {
                return None;
            }
            Some(FmtFields {
                struct_name: struct_name.clone(),
                fields: fields
                    .iter()
                    .map(|f| (f.name.clone(), f.field_index, f.value))
                    .collect(),
                struct_type: *struct_type,
                value_type_id,
                value_span,
            })
        }
        ExprKind::LabeledBlock { block, .. } => {
            // Pattern: { let buf = &mut __tmpl_buf; break label: Formatter { ..., buf } }
            // or: { __local = __tmpl_buf; break: Formatter { ..., buf: ref.as_non_null(__local) } }
            let block = *block;
            let break_stmt = *body.blocks[block].stmts.last()?;
            let break_value = match &body.stmts[break_stmt].kind {
                StmtKind::Break { value: Some(v), .. } => *v,
                _ => return None,
            };
            extract_formatter_fields_from_block(
                body,
                block,
                break_value,
                hoisted_buf_index,
                value_type_id,
                value_span,
            )
        }
        ExprKind::Block(block) => {
            // After `branch_prune`'s C3 rewrite flattens
            // `__inline_Formatter__new_*: { …; break: Formatter { … } }`
            // into a plain `Block { …; Expr(Formatter { … }) }`, the
            // surface shape `extract_formatter_fields` used to match on
            // (`LabeledBlock`) is gone. Defensively support the flattened
            // shape here so a future change to the inliner (e.g. one that
            // leaves a multi-use `buf` binding `copy_prop` can't fold) does
            // not silently disable Formatter hoisting.
            let block = *block;
            let tail_stmt = *body.blocks[block].stmts.last()?;
            let StmtKind::Expr(tail) = &body.stmts[tail_stmt].kind else {
                return None;
            };
            extract_formatter_fields_from_block(
                body,
                block,
                *tail,
                hoisted_buf_index,
                value_type_id,
                value_span,
            )
        }
        _ => None,
    }
}

/// Shared "block carrying a `Formatter { … }` value" matcher used by both
/// the `LabeledBlock` (inlined `Formatter::new`) and the post-C3 `Block`
/// (flattened version of the same shape) arms of
/// `extract_formatter_fields`. `value_expr` is the producing expression —
/// either the broken value or the trailing `Expr` stmt.
fn extract_formatter_fields_from_block(
    body: &Body,
    block: BlockId,
    value_expr: ExprId,
    hoisted_buf_index: u32,
    value_type_id: TypeId,
    value_span: Span,
) -> Option<FmtFields> {
    let ExprKind::StructLiteral {
        struct_name,
        fields,
        struct_type,
    } = &body.exprs[value_expr].kind
    else {
        return None;
    };

    let make = || FmtFields {
        struct_name: struct_name.clone(),
        fields: fields
            .iter()
            .map(|f| (f.name.clone(), f.field_index, f.value))
            .collect(),
        struct_type: *struct_type,
        value_type_id,
        value_span,
    };

    // Check if buf traces to hoisted buffer (directly or via intermediate local)
    let buf_field = fields.iter().find(|f| f.name == "buf")?;
    if buf_field_references_local(body, buf_field.value, hoisted_buf_index) {
        return Some(make());
    }

    // Trace through intermediate local in the block
    let buf_inner_local = extract_local_from_ref(body, buf_field.value)?;
    for s in &body.blocks[block].stmts {
        match &body.stmts[*s].kind {
            StmtKind::Let {
                local_index,
                value: let_value,
                ..
            } if *local_index == buf_inner_local => {
                if references_local(body, *let_value, hoisted_buf_index) {
                    return Some(make());
                }
            }
            StmtKind::Expr(expr) => {
                if let ExprKind::Assign { target, value: av } = &body.exprs[*expr].kind
                    && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                    && *index == buf_inner_local
                    && references_local(body, *av, hoisted_buf_index)
                {
                    return Some(make());
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract the local index from a reference expression.
/// Handles `&mut Local(N)`, `Local(N)`, and `ref.as_non_null(Local(N))`.
fn extract_local_from_ref(body: &Body, e: ExprId) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::MutRef | NirUnaryOp::Ref,
            expr: inner,
        } => match &body.exprs[*inner].kind {
            ExprKind::Local { index, .. } => Some(*index),
            _ => None,
        },
        ExprKind::Call { func, args, .. } if func.name.contains("ref.as_non_null") => {
            args.first().and_then(|a| match &body.exprs[a.expr].kind {
                ExprKind::Local { index, .. } => Some(*index),
                _ => None,
            })
        }
        _ => None,
    }
}

/// Check if an expression *aliases* a specific local (the local itself or a
/// `&mut` chain to it).
///
/// Intentionally narrow: matching a non-alias *mention* (e.g. `foo(buf)`) would
/// make the caller force-rewrite a Formatter's `buf` to the wrong buffer — a
/// miscompile — so this must not be widened to a "mentions anywhere" walk such
/// as `expr_mentions_local`. Pinned by the `references_local_matches_only_aliases_not_mentions`
/// unit test below.
fn references_local(body: &Body, e: ExprId, local_index: u32) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => *index == local_index,
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => references_local(body, *inner, local_index),
        _ => false,
    }
}

/// Check if a `buf` field expression references the given local (the hoisted String buffer).
/// Handles both `&mut local` (NIR form) and `ref.as_non_null(local)` patterns.
fn buf_field_references_local(body: &Body, e: ExprId, local_index: u32) -> bool {
    match &body.exprs[e].kind {
        // &mut __tmpl_buf (NIR level)
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => is_local(body, *inner, local_index),
        // ref.as_non_null(__tmpl_buf) (WIR level / after lowering)
        ExprKind::Call { func, args, .. } => {
            func.name.contains("ref.as_non_null")
                && args.len() == 1
                && is_local(body, args[0].expr, local_index)
        }
        _ => false,
    }
}

/// Check if an expression is a compile-time constant (literal).
fn is_constant_expr(body: &Body, e: ExprId) -> bool {
    matches!(
        &body.exprs[e].kind,
        ExprKind::IntLiteral { .. }
            | ExprKind::BoolLiteral(_)
            | ExprKind::CharLiteral(_)
            | ExprKind::EnumConstruct { .. }
    )
}

/// Whether `expr` is an `array_new<u8>(N)` call carrying a capacity argument.
fn array_new_has_capacity(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Call { func, args, .. } => func.name.contains("array_new") && !args.is_empty(),
        _ => false,
    }
}

/// Transform a `__tmpl` block to reuse a hoisted String.
///
/// The entire String (not just the backing array) is hoisted before the loop.
/// Inside the block, `let mut __r = String { ... }` is replaced with
/// `__tmpl_buf.used = 0` (field reset), and all references to `__r` are
/// renamed to `__tmpl_buf`. The outer Let binding gets `skip_value_copy = true`
/// so the bound variable aliases the hoisted String directly.
fn transform_tmpl_block(
    engine: &mut Engine,
    block: BlockId,
    candidate: &TmplCandidate,
    hoist_stmts: &mut Vec<StmtId>,
    type_table: &RefCell<TypeTable>,
) {
    let span = candidate.span;
    let string_type = candidate.string_type;

    // Allocate a new local for the hoisted String via the engine.
    let buf_local_name = format!("__tmpl_buf_{}", engine.locals().len());
    let buf_local_index =
        engine.alloc_local(buf_local_name.clone(), string_type, /* is_mut */ true);

    // Hoist statement: let mut __tmpl_buf_N = String { repr: array_new(N), used: 0 };
    // Reuse the original init-value subtree (its old `Let` is replaced below).
    let hoist_let = engine.alloc_stmt(
        StmtKind::Let {
            name: buf_local_name.clone(),
            local_index: buf_local_index,
            is_mut: true,
            is_reactive: false,
            type_id: string_type,
            value: candidate.init_value,
            skip_value_copy: false,
        },
        span,
    );
    hoist_stmts.push(hoist_let);

    // Replace the first statement (let mut __r = String { ... }) with a field reset:
    // __tmpl_buf_N.used = 0;
    let reset_stmt = build_field_reset(
        engine,
        buf_local_index,
        &buf_local_name,
        string_type,
        1,
        SeqField::Len.field_name(),
        span,
    );
    let mut new_stmts = engine.body.blocks[block].stmts.clone();
    new_stmts[0] = reset_stmt;
    engine.set_block_stmts(block, new_stmts);

    // Rename all references from __r (old local index) to __tmpl_buf_N (new local index)
    let old_index = candidate.buf_local_index;
    for s in engine.body.blocks[block].stmts.clone() {
        rename_local_in_stmt(engine, s, old_index, buf_local_index, &buf_local_name);
    }

    // Phase 2: Hoist Formatter struct literals as well.
    // After the String rename above, the block may contain one or more Formatter
    // creations (direct struct literals or inlined Formatter::new LabeledBlocks).
    // Each distinct Formatter is hoisted to its own local before the loop.
    let fmt_candidates = extract_fmt_candidates(engine, block, buf_local_index, type_table);
    if !fmt_candidates.is_empty() {
        transform_fmts_in_tmpl_block(engine, block, &fmt_candidates, hoist_stmts);
    }
}

/// Build a `target_local.<field> = 0` statement (an `Expr(Assign)` over a
/// `FieldAccess`), returning its arena id.
fn build_field_reset(
    engine: &mut Engine,
    local_index: u32,
    local_name: &str,
    local_type: TypeId,
    field_index: u32,
    field_name: &str,
    span: Span,
) -> StmtId {
    let local = engine.alloc_expr(
        ExprKind::Local {
            index: local_index,
            name: local_name.to_string(),
        },
        local_type,
        span,
    );
    let field = engine.alloc_expr(
        ExprKind::FieldAccess {
            expr: local,
            field_index,
            field_name: field_name.to_string(),
        },
        TypeTable::I32,
        span,
    );
    let zero = engine.alloc_expr(
        ExprKind::IntLiteral {
            value: 0,
            repr: "0".to_string(),
        },
        TypeTable::I32,
        span,
    );
    let assign = engine.alloc_expr(
        ExprKind::Assign {
            target: field,
            value: zero,
        },
        TypeTable::UNIT,
        span,
    );
    engine.alloc_stmt(StmtKind::Expr(assign), span)
}

/// Hoist Formatter struct literals out of a `__tmpl` block.
///
/// Each candidate gets its own hoisted local. Replaces the struct literal with an
/// `indent` field reset, and renames all references to the hoisted local.
///
/// Processes candidates in reverse order so that `stmt_index` values remain valid
/// as we replace statements.
fn transform_fmts_in_tmpl_block(
    engine: &mut Engine,
    block: BlockId,
    candidates: &[FmtCandidate],
    hoist_stmts: &mut Vec<StmtId>,
) {
    // Sort by stmt_index ascending to compute rename ranges
    let mut sorted_candidates: Vec<_> = candidates.iter().collect();
    sorted_candidates.sort_by_key(|c| c.stmt_index);

    // Build a mapping from each candidate to its hoisted local and rename range.
    // When multiple candidates share the same fmt_local_index, each one's rename
    // range extends from its stmt_index to the next candidate's stmt_index (exclusive)
    // that shares the same local. For the last candidate with a given local, the
    // range extends to the end of the block.
    struct HoistInfo {
        hoisted_index: u32,
        hoisted_name: String,
        stmt_index: usize,
        rename_start: usize,
        rename_end: usize, // exclusive
        old_fmt_index: u32,
        formatter_type: TypeId,
        indent_field_index: u32,
        init_value: ExprId,
        span: Span,
    }
    let block_len = engine.body.blocks[block].stmts.len();
    let mut hoist_infos = Vec::new();

    for (pos, candidate) in sorted_candidates.iter().enumerate() {
        // Keep this name aligned with the corresponding `Let` statement
        // built below. WIR naming is primarily derived from discovered
        // `Let`s by local index, with `tir_func.locals[idx].name` used as a
        // fallback when no `Let` is found, so matching names mainly
        // improves fallback / debug output consistency.
        let hoisted_name = format!("__fmt_buf_{}", engine.locals().len());
        let fmt_local_index = engine.alloc_local(
            hoisted_name.clone(),
            candidate.formatter_type,
            /* is_mut */ true,
        );

        // Find the next candidate that shares the same fmt_local_index
        let rename_end = sorted_candidates[pos + 1..]
            .iter()
            .find(|c| c.fmt_local_index == candidate.fmt_local_index)
            .map(|c| c.stmt_index)
            .unwrap_or(block_len);

        hoist_infos.push(HoistInfo {
            hoisted_index: fmt_local_index,
            hoisted_name,
            stmt_index: candidate.stmt_index,
            rename_start: candidate.stmt_index,
            rename_end,
            old_fmt_index: candidate.fmt_local_index,
            formatter_type: candidate.formatter_type,
            indent_field_index: candidate.indent_field_index,
            init_value: candidate.init_value,
            span: candidate.span,
        });
    }

    for info in &hoist_infos {
        // Hoist statement: let mut __fmt_buf_N = Formatter { fill: ..., buf: ... };
        // Reuse the normalized Formatter literal built during extraction.
        let hoist_let = engine.alloc_stmt(
            StmtKind::Let {
                name: info.hoisted_name.clone(),
                local_index: info.hoisted_index,
                is_mut: true,
                is_reactive: false,
                type_id: info.formatter_type,
                value: info.init_value,
                skip_value_copy: false,
            },
            info.span,
        );
        hoist_stmts.push(hoist_let);

        // Replace the Formatter struct literal with an indent field reset:
        //   __fmt_buf_N.indent = 0;
        //
        // IMPORTANT: Do NOT remove this indent = 0 reset! Format functions
        // (especially pretty-print with `:#?`) may modify the `indent` field
        // during formatting. Without this reset, the indent value would
        // accumulate across loop iterations, causing incorrect indentation.
        let indent_reset = build_field_reset(
            engine,
            info.hoisted_index,
            &info.hoisted_name,
            info.formatter_type,
            info.indent_field_index,
            "indent",
            info.span,
        );
        let mut new_stmts = engine.body.blocks[block].stmts.clone();
        new_stmts[info.stmt_index] = indent_reset;
        engine.set_block_stmts(block, new_stmts);

        // Rename references from the old Formatter local to the hoisted one,
        // only within [rename_start, rename_end) to avoid clobbering other
        // candidates that share the same original local.
        let range_stmts: Vec<StmtId> =
            engine.body.blocks[block].stmts[info.rename_start..info.rename_end].to_vec();
        for s in range_stmts {
            rename_local_in_stmt(
                engine,
                s,
                info.old_fmt_index,
                info.hoisted_index,
                &info.hoisted_name,
            );
        }
    }
}

fn rename_local_in_stmt(
    engine: &mut Engine,
    s: StmtId,
    old_index: u32,
    new_index: u32,
    new_name: &str,
) {
    enum Shape {
        Expr(ExprId),
        If(ExprId, BlockId, Option<BlockId>),
        Block(BlockId),
        None,
    }
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::Let { value, .. } => Shape::Expr(*value),
        StmtKind::Expr(expr) => Shape::Expr(*expr),
        StmtKind::Return { value: Some(expr) } => Shape::Expr(*expr),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => Shape::If(*condition, *then_block, *else_block),
        StmtKind::LabeledBlock { block, .. } => Shape::Block(*block),
        StmtKind::Loop { body } => Shape::Block(*body),
        StmtKind::Break {
            value: Some(expr), ..
        } => Shape::Expr(*expr),
        _ => Shape::None,
    };
    match shape {
        Shape::Expr(e) => rename_local_in_expr(engine, e, old_index, new_index, new_name),
        Shape::If(cond, tb, eb) => {
            rename_local_in_expr(engine, cond, old_index, new_index, new_name);
            rename_local_in_block(engine, tb, old_index, new_index, new_name);
            if let Some(eb) = eb {
                rename_local_in_block(engine, eb, old_index, new_index, new_name);
            }
        }
        Shape::Block(b) => rename_local_in_block(engine, b, old_index, new_index, new_name),
        Shape::None => {}
    }
}

fn rename_local_in_block(
    engine: &mut Engine,
    block: BlockId,
    old_index: u32,
    new_index: u32,
    new_name: &str,
) {
    for s in engine.body.blocks[block].stmts.clone() {
        rename_local_in_stmt(engine, s, old_index, new_index, new_name);
    }
}

fn rename_local_in_expr(
    engine: &mut Engine,
    e: ExprId,
    old_index: u32,
    new_index: u32,
    new_name: &str,
) {
    enum Walk {
        Local,
        Exprs(Vec<ExprId>),
        CondBlocks(ExprId, BlockId, Option<BlockId>),
        Block(BlockId),
        None,
    }
    let walk = match &engine.body.exprs[e].kind {
        ExprKind::Local { index, .. } if *index == old_index => Walk::Local,
        ExprKind::Call { args, .. } => Walk::Exprs(args.iter().map(|a| a.expr).collect()),
        ExprKind::MethodCall { receiver, args, .. } => {
            let mut v = vec![*receiver];
            v.extend(args.iter().map(|a| a.expr));
            Walk::Exprs(v)
        }
        ExprKind::IndirectCall { callee, args } => {
            let mut v = vec![*callee];
            v.extend(args.iter().copied());
            Walk::Exprs(v)
        }
        ExprKind::Binary { left, right, .. } => Walk::Exprs(vec![*left, *right]),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. } => Walk::Exprs(vec![*inner]),
        ExprKind::Assign { target, value } => Walk::Exprs(vec![*target, *value]),
        ExprKind::StructLiteral { fields, .. } => {
            Walk::Exprs(fields.iter().map(|f| f.value).collect())
        }
        ExprKind::Index { expr: inner, index } => Walk::Exprs(vec![*inner, *index]),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => Walk::CondBlocks(*condition, *then_branch, *else_branch),
        ExprKind::LabeledBlock { block, .. } | ExprKind::Block(block) => Walk::Block(*block),
        _ => Walk::None,
    };
    match walk {
        Walk::Local => {
            // The Local kind's `index` field is what the engine's use index
            // is keyed on, so route the rename through `replace_expr_kind`
            // to drop the old `old_index` mention and register a new
            // `new_index` mention.
            engine.replace_expr_kind(
                e,
                ExprKind::Local {
                    index: new_index,
                    name: new_name.to_string(),
                },
            );
        }
        Walk::Exprs(v) => {
            for id in v {
                rename_local_in_expr(engine, id, old_index, new_index, new_name);
            }
        }
        Walk::CondBlocks(cond, tb, eb) => {
            rename_local_in_expr(engine, cond, old_index, new_index, new_name);
            rename_local_in_block(engine, tb, old_index, new_index, new_name);
            if let Some(eb) = eb {
                rename_local_in_block(engine, eb, old_index, new_index, new_name);
            }
        }
        Walk::Block(b) => rename_local_in_block(engine, b, old_index, new_index, new_name),
        Walk::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::tir::TypeId;

    /// Build a `Body` whose root holds a single expression (built by `build`)
    /// and return its arena id so the arena-side `references_local` can be
    /// exercised directly.
    fn body_of(build: impl FnOnce(&mut Body) -> ExprId) -> (Body, ExprId) {
        let mut body = Body::empty();
        let e = build(&mut body);
        let s = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(e),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s],
            span: Span::default(),
        });
        (body, e)
    }

    fn local(body: &mut Body, idx: u32) -> ExprId {
        body.exprs.push(ExprNode {
            kind: ExprKind::Local {
                index: idx,
                name: format!("__l{idx}"),
            },
            type_id: TypeId(0),
            span: Span::default(),
        })
    }

    fn unary(body: &mut Body, op: NirUnaryOp, inner: ExprId) -> ExprId {
        body.exprs.push(ExprNode {
            kind: ExprKind::Unary { op, expr: inner },
            type_id: TypeId(0),
            span: Span::default(),
        })
    }

    /// `foo(arg)` — a free-function call carrying `arg`. The result is an
    /// independent value, so the call only *mentions* `arg`, it is not an
    /// alias of it.
    fn call_with(body: &mut Body, arg: ExprId) -> ExprId {
        use crate::module_source::ModuleSource;
        use crate::nir::FunctionRef;
        use crate::nir_arena::ArenaCallArg;
        body.exprs.push(ExprNode {
            kind: ExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::entry_point_synthetic(),
                    name: "foo".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: vec![],
                args: vec![ArenaCallArg {
                    expr: arg,
                    is_mut: false,
                }],
            },
            type_id: TypeId(0),
            span: Span::default(),
        })
    }

    /// `references_local` decides whether an intermediate `let buf_inner = <e>`
    /// binding makes `buf_inner` an *alias* of the hoisted template buffer.
    /// When it answers yes, `extract_fmt_candidates` force-rewrites the matched
    /// Formatter's `buf` field to `&mut <hoisted buffer>` — so a false positive
    /// redirects the Formatter to the wrong buffer (a miscompile).
    ///
    /// It must therefore match only genuine alias shapes — a bare `Local` or a
    /// `&mut` chain down to it — and reject any expression that merely *mentions*
    /// the local (a call argument, an operand, …). Broadening it to a full
    /// "mentions anywhere" walk (e.g. `expr_mentions_local`) reintroduces that
    /// miscompile; these assertions exist to fail the moment someone does.
    #[test]
    fn references_local_matches_only_aliases_not_mentions() {
        const IDX: u32 = 7;

        // Genuine aliases — must match.
        let (b, e) = body_of(|b| local(b, IDX));
        assert!(references_local(&b, e, IDX));
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            unary(b, NirUnaryOp::MutRef, l)
        });
        assert!(references_local(&b, e, IDX));
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            let inner = unary(b, NirUnaryOp::MutRef, l);
            unary(b, NirUnaryOp::MutRef, inner)
        });
        assert!(references_local(&b, e, IDX));

        // A different local is unrelated.
        let (b, e) = body_of(|b| local(b, IDX + 1));
        assert!(!references_local(&b, e, IDX));

        // Non-alias *mentions* — must NOT match (this is the guard against
        // `expr_mentions_local`, which would return true for all of these and
        // miscompile the Formatter buffer rewrite).
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            call_with(b, l)
        });
        assert!(!references_local(&b, e, IDX));
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            let c = call_with(b, l);
            unary(b, NirUnaryOp::MutRef, c)
        });
        assert!(!references_local(&b, e, IDX));
        // `&buf` (shared ref) is not the `&mut` alias shape the hoist normalizes.
        let (b, e) = body_of(|b| {
            let l = local(b, IDX);
            unary(b, NirUnaryOp::Ref, l)
        });
        assert!(!references_local(&b, e, IDX));
    }
}
