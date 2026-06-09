//! Store-to-Load Forwarding optimization for Wado NIR
//!
//! When a literal value is stored to a local and then loaded with no
//! intervening aliasing write, this pass forwards the stored value directly,
//! eliminating the load. Particularly useful after SROA decomposes struct
//! fields into scalar locals.
//!
//! Safety: a local is excluded from forwarding when its address is taken
//! (`&x` / `&mut x`) or it is captured by a closure. At control-flow
//! boundaries only locals modified within branches are invalidated.
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and runs the whole-function flow-sensitive forward pass.
//! The single rewrite point (`Local → ForwardValue`-derived literal) routes
//! through the engine edit API (`replace_expr_kind`) so the parent map and
//! use index stay coherent.

use std::cell::Cell;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

/// Precomputed per-block modified-locals cache, keyed by `BlockId`.
type ModifiedLocalsCache = IndexMap<BlockId, IndexSet<u32>>;

/// Insert the assigned local for an `Assign` target (bare `Local` or
/// `FieldAccess(Local)`), matching the original modified-locals logic.
fn insert_assign_target(body: &Body, target: ExprId, modified: &mut IndexSet<u32>) {
    if let ExprKind::Local { index, .. } = &body.exprs[target].kind {
        modified.insert(*index);
    }
    if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[target].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
    {
        modified.insert(*index);
    }
}

fn precompute_all_modified_locals(body: &Body) -> ModifiedLocalsCache {
    let mut cache = ModifiedLocalsCache::default();
    precompute_block(body, body.root, &mut cache);
    cache
}

fn precompute_block(body: &Body, block: BlockId, cache: &mut ModifiedLocalsCache) -> IndexSet<u32> {
    let mut modified = IndexSet::default();
    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        precompute_node(body, NodeRef::Stmt(s), &mut modified, cache);
    }
    cache.insert(block, modified.clone());
    modified
}

fn precompute_node(
    body: &Body,
    node: NodeRef,
    modified: &mut IndexSet<u32>,
    cache: &mut ModifiedLocalsCache,
) {
    match node {
        NodeRef::Block(b) => {
            let inner = precompute_block(body, b, cache);
            modified.extend(inner);
        }
        NodeRef::Expr(id) => {
            if let ExprKind::Assign { target, .. } = &body.exprs[id].kind {
                let target = *target;
                insert_assign_target(body, target, modified);
            }
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                precompute_node(body, c, modified, cache);
            }
        }
        NodeRef::Stmt(s) => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Stmt(s), |c| kids.push(c));
            for c in kids {
                precompute_node(body, c, modified, cache);
            }
        }
        NodeRef::Pat(_) => {}
    }
}

/// Collect assign-target locals in an expression subtree (no caching). Used for
/// `Match` arms, whose bodies are expressions (not blocks).
fn collect_modified_expr(body: &Body, expr: ExprId) -> IndexSet<u32> {
    let mut modified = IndexSet::default();
    collect_modified_node(body, NodeRef::Expr(expr), &mut modified);
    modified
}

fn collect_modified_node(body: &Body, node: NodeRef, modified: &mut IndexSet<u32>) {
    if let NodeRef::Expr(id) = node
        && let ExprKind::Assign { target, .. } = &body.exprs[id].kind
    {
        let target = *target;
        insert_assign_target(body, target, modified);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_modified_node(body, c, modified);
    }
}

/// A forwardable value: a scalar literal.
#[derive(Debug, Clone)]
enum ForwardValue {
    Int { value: u64, repr: String },
    Float { value: f64, repr: String },
    Bool(bool),
    Char(char),
}

impl ForwardValue {
    fn from_expr(body: &Body, expr: ExprId) -> Option<Self> {
        match &body.exprs[expr].kind {
            ExprKind::IntLiteral { value, repr } => Some(Self::Int {
                value: *value,
                repr: repr.clone(),
            }),
            ExprKind::FloatLiteral { value, repr } => Some(Self::Float {
                value: *value,
                repr: repr.clone(),
            }),
            ExprKind::BoolLiteral(b) => Some(Self::Bool(*b)),
            ExprKind::CharLiteral(c) => Some(Self::Char(*c)),
            _ => None,
        }
    }

    fn to_expr_kind(&self) -> ExprKind {
        match self {
            Self::Int { value, repr } => ExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Float { value, repr } => ExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            Self::Bool(b) => ExprKind::BoolLiteral(*b),
            Self::Char(c) => ExprKind::CharLiteral(*c),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct KnownValues {
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
    fn invalidate_modified(&mut self, modified: &IndexSet<u32>) {
        for &idx in modified {
            self.invalidate_local(idx);
        }
    }
}

/// Collect locals that are unsafe for forwarding: address taken (`&` / `&mut`).
fn collect_unsafe_locals(body: &Body) -> IndexSet<u32> {
    let mut unsafe_locals = IndexSet::default();
    collect_unsafe_node(body, NodeRef::Block(body.root), &mut unsafe_locals);
    unsafe_locals
}

fn collect_unsafe_node(body: &Body, node: NodeRef, unsafe_locals: &mut IndexSet<u32>) {
    if let NodeRef::Expr(id) = node
        && let ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } = &body.exprs[id].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
    {
        unsafe_locals.insert(*index);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_unsafe_node(body, c, unsafe_locals);
    }
}

pub fn forward_stores_to_loads(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::StoreLoadForward, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = StoreLoadForwardRule {
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
/// function flow-sensitive forward pass at the body root.
pub(super) struct StoreLoadForwardRule<'a> {
    type_table: &'a TypeTable,
    applied: Cell<bool>,
}

impl Rule for StoreLoadForwardRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        forward_at_root(engine, self.type_table)
    }
}

fn forward_at_root(engine: &mut Engine, type_table: &TypeTable) -> bool {
    let unsafe_locals = collect_unsafe_locals(engine.body);
    let cache = precompute_all_modified_locals(engine.body);
    let mut known = KnownValues::default();
    let root = engine.body.root;
    forward_in_block(engine, root, &mut known, &unsafe_locals, type_table, &cache)
}

fn lookup_modified(cache: &ModifiedLocalsCache, block: BlockId) -> IndexSet<u32> {
    cache.get(&block).cloned().unwrap_or_default()
}

fn forward_in_block(
    engine: &mut Engine,
    block: BlockId,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    cache: &ModifiedLocalsCache,
) -> bool {
    let mut changed = false;
    let stmts = engine.body.blocks[block].stmts.clone();
    for stmt in stmts {
        changed |= forward_in_stmt(engine, stmt, known, unsafe_locals, type_table, cache);
    }
    changed
}

fn forward_in_stmt(
    engine: &mut Engine,
    stmt: StmtId,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    cache: &ModifiedLocalsCache,
) -> bool {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let (local_index, value) = (*local_index, *value);
            let changed = forward_in_expr(engine,value, known, unsafe_locals, type_table, cache);
            if !unsafe_locals.contains(&local_index)
                && let Some(fv) = ForwardValue::from_expr(engine.body, value)
            {
                known.set_local(local_index, fv);
            }
            changed
        }
        StmtKind::Expr(e) => {
            let e = *e;
            let changed = forward_in_expr(engine,e, known, unsafe_locals, type_table, cache);
            update_known_from_assign(engine.body, e, known, unsafe_locals);
            changed
        }
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            if let Some(v) = *value {
                forward_in_expr(engine,v, known, unsafe_locals, type_table, cache)
            } else {
                false
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
            let mut changed =
                forward_in_expr(engine,condition, known, unsafe_locals, type_table, cache);
            let mut modified = lookup_modified(cache, then_block);
            if let Some(eb) = else_block {
                modified.extend(lookup_modified(cache, eb));
            }
            let mut then_known = known.clone();
            changed |= forward_in_block(
                engine,
                then_block,
                &mut then_known,
                unsafe_locals,
                type_table,
                cache,
            );
            if let Some(eb) = else_block {
                let mut else_known = known.clone();
                changed |=
                    forward_in_block(engine,eb, &mut else_known, unsafe_locals, type_table, cache);
            }
            known.invalidate_modified(&modified);
            changed
        }
        StmtKind::Loop { body: b } => {
            let b = *b;
            let modified = lookup_modified(cache, b);
            known.invalidate_modified(&modified);
            let mut loop_known = known.clone();
            let changed =
                forward_in_block(engine,b, &mut loop_known, unsafe_locals, type_table, cache);
            known.invalidate_modified(&modified);
            changed
        }
        StmtKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            let modified = lookup_modified(cache, b);
            let changed = forward_in_block(engine,b, known, unsafe_locals, type_table, cache);
            known.invalidate_modified(&modified);
            changed
        }
        StmtKind::Continue => false,
        StmtKind::LetDestructure { value, .. } => {
            let value = *value;
            forward_in_expr(engine,value, known, unsafe_locals, type_table, cache)
        }
    }
}

fn forward_in_expr(
    engine: &mut Engine,
    id: ExprId,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
    type_table: &TypeTable,
    cache: &ModifiedLocalsCache,
) -> bool {
    let mut changed = try_forward_expr(engine, id, known);

    match &engine.body.exprs[id].kind {
        ExprKind::Binary { left, right, .. } => {
            let (left, right) = (*left, *right);
            changed |= forward_in_expr(engine,left, known, unsafe_locals, type_table, cache);
            changed |= forward_in_expr(engine,right, known, unsafe_locals, type_table, cache);
        }
        ExprKind::Unary { expr: inner, .. } => {
            let inner = *inner;
            changed |= forward_in_expr(engine,inner, known, unsafe_locals, type_table, cache);
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            changed |= forward_in_expr(engine, value, known, unsafe_locals, type_table, cache);
            update_known_from_target(engine.body, target, value, known, unsafe_locals);
        }
        ExprKind::Call { args, .. } => {
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for a in args {
                changed |= forward_in_expr(engine,a, known, unsafe_locals, type_table, cache);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let args = args.clone();
            for a in args {
                changed |= forward_in_expr(engine,a, known, unsafe_locals, type_table, cache);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            changed |= forward_in_expr(engine,receiver, known, unsafe_locals, type_table, cache);
            for a in args {
                changed |= forward_in_expr(engine,a, known, unsafe_locals, type_table, cache);
            }
        }
        ExprKind::IndirectCall { callee, args, .. } => {
            let callee = *callee;
            let args = args.clone();
            changed |= forward_in_expr(engine,callee, known, unsafe_locals, type_table, cache);
            for a in args {
                changed |= forward_in_expr(engine,a, known, unsafe_locals, type_table, cache);
            }
        }
        ExprKind::ClosureToCanonical { functor, .. } => {
            let functor = *functor;
            changed |= forward_in_expr(engine,functor, known, unsafe_locals, type_table, cache);
        }
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            let inner = *inner;
            changed |= forward_in_expr(engine,inner, known, unsafe_locals, type_table, cache);
        }
        ExprKind::Index { expr: inner, index } => {
            let (inner, index) = (*inner, *index);
            changed |= forward_in_expr(engine,inner, known, unsafe_locals, type_table, cache);
            changed |= forward_in_expr(engine,index, known, unsafe_locals, type_table, cache);
        }
        ExprKind::Block(b) => {
            let b = *b;
            changed |= forward_in_block(engine,b, known, unsafe_locals, type_table, cache);
        }
        ExprKind::LabeledBlock { block: b, .. } => {
            let b = *b;
            let modified = lookup_modified(cache, b);
            changed |= forward_in_block(engine,b, known, unsafe_locals, type_table, cache);
            known.invalidate_modified(&modified);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            changed |= forward_in_expr(engine,condition, known, unsafe_locals, type_table, cache);
            let mut modified = lookup_modified(cache, then_branch);
            if let Some(eb) = else_branch {
                modified.extend(lookup_modified(cache, eb));
            }
            let mut then_known = known.clone();
            changed |= forward_in_block(
                engine,
                then_branch,
                &mut then_known,
                unsafe_locals,
                type_table,
                cache,
            );
            if let Some(eb) = else_branch {
                let mut else_known = known.clone();
                changed |=
                    forward_in_block(engine,eb, &mut else_known, unsafe_locals, type_table, cache);
            }
            known.invalidate_modified(&modified);
        }
        ExprKind::StructLiteral { fields, .. } => {
            let vals: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            for v in vals {
                changed |= forward_in_expr(engine,v, known, unsafe_locals, type_table, cache);
            }
        }
        ExprKind::TupleLiteral { elements, .. } | ExprKind::ArrayLiteral { elements, .. } => {
            let elems = elements.clone();
            for e in elems {
                changed |= forward_in_expr(engine,e, known, unsafe_locals, type_table, cache);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = *payload {
                changed |= forward_in_expr(engine,p, known, unsafe_locals, type_table, cache);
            }
        }
        ExprKind::Match { expr: inner, arms } => {
            let inner = *inner;
            let arm_data: Vec<(Option<ExprId>, ExprId)> =
                arms.iter().map(|a| (a.guard, a.body)).collect();
            changed |= forward_in_expr(engine, inner, known, unsafe_locals, type_table, cache);
            let mut modified = IndexSet::default();
            for (guard, arm_body) in &arm_data {
                if let Some(g) = guard {
                    modified.extend(collect_modified_expr(engine.body, *g));
                }
                modified.extend(collect_modified_expr(engine.body, *arm_body));
            }
            for (guard, arm_body) in arm_data {
                let mut arm_known = known.clone();
                if let Some(g) = guard {
                    changed |=
                        forward_in_expr(engine, g, &mut arm_known, unsafe_locals, type_table, cache);
                }
                changed |= forward_in_expr(
                    engine,
                    arm_body,
                    &mut arm_known,
                    unsafe_locals,
                    type_table,
                    cache,
                );
            }
            known.invalidate_modified(&modified);
        }
        ExprKind::GlobalVarSet { value, .. } => {
            let value = *value;
            changed |= forward_in_expr(engine,value, known, unsafe_locals, type_table, cache);
        }
        ExprKind::VariantTag { expr }
        | ExprKind::VariantTest { expr, .. }
        | ExprKind::VariantPayload { expr, .. } => {
            let expr = *expr;
            changed |= forward_in_expr(engine,expr, known, unsafe_locals, type_table, cache);
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
            changed |= forward_in_expr(engine,scrutinee, known, unsafe_locals, type_table, cache);
            let mut modified = IndexSet::default();
            for &arm in &arms {
                modified.extend(lookup_modified(cache, arm));
            }
            modified.extend(lookup_modified(cache, default));
            for arm in arms {
                let mut arm_known = known.clone();
                changed |=
                    forward_in_block(engine,arm, &mut arm_known, unsafe_locals, type_table, cache);
            }
            let mut default_known = known.clone();
            changed |= forward_in_block(
                engine,
                default,
                &mut default_known,
                unsafe_locals,
                type_table,
                cache,
            );
            known.invalidate_modified(&modified);
        }
        _ => {}
    }

    changed
}

fn try_forward_expr(engine: &mut Engine, id: ExprId, known: &KnownValues) -> bool {
    let fv = if let ExprKind::Local { index, .. } = &engine.body.exprs[id].kind {
        known.get_local(*index).cloned()
    } else {
        None
    };
    if let Some(fv) = fv {
        engine.replace_expr_kind(id, fv.to_expr_kind());
        return true;
    }
    false
}

fn update_known_from_target(
    body: &Body,
    target: ExprId,
    value: ExprId,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
) {
    if let ExprKind::Local { index, .. } = &body.exprs[target].kind {
        let idx = *index;
        if unsafe_locals.contains(&idx) {
            return;
        }
        if let Some(fv) = ForwardValue::from_expr(body, value) {
            known.set_local(idx, fv);
        } else {
            known.invalidate_local(idx);
        }
    }
}

fn update_known_from_assign(
    body: &Body,
    expr: ExprId,
    known: &mut KnownValues,
    unsafe_locals: &IndexSet<u32>,
) {
    if let ExprKind::Assign { target, value } = &body.exprs[expr].kind {
        let (target, value) = (*target, *value);
        update_known_from_target(body, target, value, known, unsafe_locals);
    }
}
