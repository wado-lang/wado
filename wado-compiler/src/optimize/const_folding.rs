//! Constant folding optimization for Wado NIR.
//!
//! Walks every function's arena [`Body`] and applies the
//! [`niri::Interpreter`] rewrite rules at each visited node. All
//! reduction logic (literal folding, integer cast collapsing,
//! short-circuit identity rules, env-aware local lookup) lives in
//! [`crate::niri`]; this module is only the visitor glue that drives
//! `reduce_local_a` across function bodies and feeds the interpreter's
//! local-variable env from `Let` / `Assign` statements.
//!
//! This walker tracks only scalar local lattices; reaching-def of a struct
//! field `obj.f` is the engine [`ValueGraph`]'s job (store-load forwarding +
//! hash-cons CSE). The local env needs no branch fork because mutable locals
//! are recorded [`Lattice::NonConst`] up front.
//!
//! The visitor mutates the arena `Body` directly: the per-node rewrites
//! (`reduce_local_a`) and the block-level branch splice
//! (`reduce_local_block_a`) operate on arena ids. Global initializers are
//! arena `ExprBody`s too, so the global env / global-field env are read from
//! them via the arena interpreter path.
//!
//! [`ValueGraph`]: crate::nir_value_graph

use std::cell::RefCell;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, FunctionId, GatedPass};
use crate::compiler_item::SeqField;
use crate::const_eval::Value;
use crate::hashmap::IndexSet;
use crate::nir::NirFunction;
use crate::nir::{FunctionRef, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::niri::{
    CalleeMap, EditSink, GlobalEnv, GlobalFieldEnv, GlobalKey, Interpreter, Lattice,
    is_ctfe_eligible,
};
use crate::tir::{PrimitiveType, TypeId, TypeTable};

/// Apply constant folding to all functions in the project.
/// Flow-sensitive constant folding, gated: skips functions unchanged since this
/// pass last ran. Used in the fixed-point loop.
pub fn fold_constants(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    fold_constants_impl(project, Some(gate))
}

/// Ungated variant: folds every function. Used by the post-globalization
/// cleanup, which runs to its own fixed point outside the gated loop.
pub fn fold_constants_all(project: &mut NirPackage) -> bool {
    fold_constants_impl(project, None)
}

fn fold_constants_impl(project: &mut NirPackage, mut gate: Option<&mut FunctionGate>) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    // Build the CalleeMap once per pass with Rc handles aliased with
    // `project.functions`. The interpreter reads callee bodies via
    // `try_borrow`, which bails cleanly when the visitor already
    // holds `borrow_mut` on the same function (the case where we'd
    // try to fold a self-call inside the function being walked).
    let callees = build_callee_map(project);
    // Build the GlobalEnv once per pass: every immutable global whose
    // initializer reduces to a `Const(_)` becomes a `GlobalVarGet`
    // rewrite target; mutable globals are recorded as `NonConst`.
    let globals = build_global_env(project, &type_table, &callees);
    // Known constant fields of immutable globals — currently the `SeqField::Len`
    // length of sequence globals hoisted by body globalization, so a
    // `global:X.used` read folds and the bounds-check / branch passes can drop
    // the checks they eliminate on the pre-hoist local form.
    let global_fields = build_global_field_env(project);
    let mut visitor = ConstFoldVisitor {
        interpreter: Interpreter::new(&type_table),
    };
    visitor.interpreter.with_callees(&callees);
    visitor.interpreter.with_globals(&globals);
    visitor.interpreter.with_global_fields(&global_fields);
    let mut buffers = EngineBuffers::default();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let fid = FunctionId::new(i);
        if let Some(g) = gate.as_deref_mut()
            && !g.needs(GatedPass::ConstFold, fid)
        {
            continue;
        }
        let mut func = func_rc.borrow_mut();
        let NirFunction { body, locals, .. } = &mut *func;
        let func_changed = if let Some(body) = body.as_mut() {
            // Local indices are unique per function, not project-wide,
            // so reset the interpreter's env at every function boundary.
            visitor.interpreter.enter_function();
            // Drive the flow-sensitive walk over an engine session so every
            // rewrite commits coherently (the engine is the commit mechanism;
            // the visitor still drives the bottom-up program-order walk).
            let mut engine = Engine::new(body, &mut buffers, locals);
            let root = engine.body.root;
            visitor.visit_block(&mut engine, root)
        } else {
            false
        };
        drop(func);
        if let Some(g) = gate.as_deref_mut() {
            g.seen(GatedPass::ConstFold, fid);
            if func_changed {
                g.mark_changed(fid);
            }
        }
        changed |= func_changed;
    }
    changed
}

/// Engine rule: environment-free constant folding.
///
/// Runs the [`Interpreter::const_fold_kind_a`] subset — literal arithmetic and
/// pure CTFE — over the worklist rewrite engine, applying each fold through the
/// engine's edit API so the parent map and use index stay coherent. The
/// program-wide [`CalleeMap`] is installed; the per-function `env` stays empty,
/// so the flow-sensitive folds (env-bound locals, immutable globals,
/// constant-branch collapse) remain with the standalone [`fold_constants`]
/// walker that still runs once per fixed-point iteration.
///
/// `const_fold_kind_a` needs `&mut Interpreter` (CTFE advances the call stack
/// and step budget), but [`Rule::apply_expr`] is `&self`, so the interpreter
/// lives behind a [`RefCell`].
pub(super) struct ConstFoldRule<'a> {
    interpreter: RefCell<Interpreter<'a>>,
}

impl<'a> ConstFoldRule<'a> {
    pub(super) fn new(type_table: &'a TypeTable, callees: &'a CalleeMap) -> Self {
        let mut interpreter = Interpreter::new(type_table);
        interpreter.with_callees(callees);
        Self {
            interpreter: RefCell::new(interpreter),
        }
    }
}

impl Rule for ConstFoldRule<'_> {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let Some(value) = self
            .interpreter
            .borrow_mut()
            .const_fold_value_a(engine.body, id)
        else {
            return false;
        };
        // Promote the folded scalar to an `Operand::Value` in its parent (WEP:
        // The Live ValueGraph). The fallback keeps the skeleton form for the rare
        // node with no operand parent slot.
        if !engine.replace_expr_with_value(id, value) {
            engine.replace_expr_kind(id, crate::const_eval::value_to_arena_kind(value));
        }
        true
    }
}

/// Engine-routed [`EditSink`]: the flow-sensitive const-fold visitor commits
/// every niri rewrite through `Engine::*` so the real body's parent map and
/// use index stay coherent (the visitor walks bottom-up in its own order; the
/// engine is the commit mechanism, not the driver).
struct EngineSink<'e, 'a> {
    engine: &'e mut Engine<'a>,
}

impl EditSink for EngineSink<'_, '_> {
    fn body(&self) -> &Body {
        self.engine.body
    }
    fn replace_kind(&mut self, e: ExprId, kind: ExprKind) {
        self.engine.replace_expr_kind(e, kind);
    }
    fn replace_with_value(&mut self, e: ExprId, value: crate::const_eval::Value) -> bool {
        self.engine.replace_expr_with_value(e, value)
    }
    fn become_expr(&mut self, dst: ExprId, src: ExprId) {
        self.engine.become_expr(dst, src);
    }
    fn alloc_expr(&mut self, kind: ExprKind, type_id: TypeId, span: crate::token::Span) -> ExprId {
        self.engine.alloc_expr(kind, type_id, span)
    }
    fn alloc_stmt(&mut self, kind: StmtKind, span: crate::token::Span) -> StmtId {
        self.engine.alloc_stmt(kind, span)
    }
    fn alloc_block(
        &mut self,
        stmts: Vec<StmtId>,
        span: crate::token::Span,
    ) -> crate::nir_arena::BlockId {
        self.engine.alloc_block(stmts, span)
    }
    fn set_block_stmts(&mut self, block: crate::nir_arena::BlockId, stmts: Vec<StmtId>) {
        self.engine.set_block_stmts(block, stmts);
    }
}

/// Pre-build the [`CalleeMap`] from every CTFE-eligible function in
/// `project`. The map stores `Rc<RefCell<NirFunction>>` handles
/// aliased with `project.functions`, so rebuilding the map every
/// optimizer iteration costs only refcount bumps. The key shape
/// `(module_source, full_name)` mirrors what `try_call_fold`
/// synthesises from a `Call` node's `FunctionRef`.
pub(super) fn build_callee_map(project: &NirPackage) -> CalleeMap {
    let mut map = CalleeMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if !is_ctfe_eligible(&func) {
            continue;
        }
        let module_source = func.module_source.clone();
        let full_name = FunctionRef::from_resolved(&func, module_source.clone()).full_name();
        drop(func);
        map.insert((module_source, full_name), func_rc.clone());
    }
    map
}

/// Pre-build the [`GlobalEnv`] from every global in `project`. Each
/// non-`mut` global's initializer is reduced through a fresh
/// [`Interpreter`] (with `callees` installed so calls in initializers
/// fold, and with the partially-built env installed so a later global
/// initializer can read constants computed from earlier ones).
/// Mutable globals are recorded as `NonConst` so a parent fold like
/// `GLOBAL_MUT + 1` correctly reports `NonConst` instead of
/// `Unevaluated`. Globals whose initializer doesn't reduce are left
/// out of the map (absent → `Lattice::Unevaluated` by default).
///
/// Global initializers are arena `ExprBody`s, so this reduces them on the
/// arena [`Interpreter::reduce_to_lattice_a`] path.
fn build_global_env(
    project: &NirPackage,
    type_table: &TypeTable,
    callees: &CalleeMap,
) -> GlobalEnv {
    let mut env = GlobalEnv::default();
    for global in &project.globals {
        let key = (global.module_source.clone(), global.name.clone());
        let lattice = if global.mutable {
            Lattice::NonConst
        } else {
            // The initializer runs at module scope: no local env, but
            // it may call pure functions and read previously-declared
            // globals. Threading `&env` in lets `global B = A + 1;`
            // fold once `A` has been recorded earlier in this loop.
            let mut interp = Interpreter::new(type_table);
            interp.with_callees(callees);
            interp.with_globals(&env);
            interp.reduce_to_lattice_a(global.initializer.body(), global.initializer.expr())
        };
        if !matches!(lattice, Lattice::Unevaluated) {
            env.insert(key, lattice);
        }
    }
    env
}

/// Arena counterpart of [`const_seq_len`]: the statically-known
/// [`SeqField::Len`] length of a constant `List` / `String` value held
/// in the arena. Used by [`SeqLenCollector`] to read the value of an
/// inline `GlobalVarSet(X, <const>)` directly from the function's arena
/// body.
/// The integer value of an operand: an `IntLiteral` expr or a promoted
/// `ValueKind::Int`.
fn operand_int_a(body: &Body, op: Operand) -> Option<u64> {
    match op {
        Operand::Expr(e) => match &body.exprs[e].kind {
            ExprKind::IntLiteral { value, .. } => Some(*value),
            _ => None,
        },
        Operand::Value(v) => match body.values.kind(v) {
            crate::nir_value_graph::ValueKind::Int(x) => Some(*x),
            _ => None,
        },
    }
}

fn const_seq_len_operand_a(body: &Body, op: Operand) -> Option<i32> {
    op.as_expr().and_then(|e| const_seq_len_a(body, e))
}

fn const_seq_len_a(body: &Body, e: ExprId) -> Option<i32> {
    match &body.exprs[e].kind {
        ExprKind::ArrayLiteral { elements } => i32::try_from(elements.len()).ok(),
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            let block = *b;
            body.blocks[block]
                .stmts
                .iter()
                .rev()
                .find_map(|s| match &body.stmts[*s].kind {
                    StmtKind::Let { value, .. } => const_seq_len_operand_a(body, *value),
                    StmtKind::Expr(ex) => const_seq_len_operand_a(body, *ex),
                    _ => None,
                })
        }
        ExprKind::StructLiteral { fields, .. } => fields.iter().find_map(|f| {
            if f.name == SeqField::Len.field_name()
                && let Some(value) = operand_int_a(body, f.value)
            {
                return i32::try_from(value).ok();
            }
            None
        }),
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            const_seq_len_operand_a(body, *inner)
        }
        _ => None,
    }
}

/// Pre-build the [`GlobalFieldEnv`]: the statically-known [`SeqField::Len`]
/// length of every Wado-immutable sequence global. Body globalization hoists
/// constant read-only `List` / `String` bindings into such globals (leaving a
/// `null` placeholder initializer and an inline `GlobalVarSet` with the real
/// value), so folding `global:X.used` to a constant lets the bounds-check /
/// branch passes recover the elimination they perform on the pre-hoist local.
fn build_global_field_env(project: &NirPackage) -> GlobalFieldEnv {
    let immutable: IndexSet<GlobalKey> = project
        .globals
        .iter()
        .filter(|g| !g.wado_mutable)
        .map(|g| (g.module_source.clone(), g.name.clone()))
        .collect();
    let mut env = GlobalFieldEnv::default();
    if immutable.is_empty() {
        return env;
    }
    // A non-placeholder const initializer (a user const sequence global) is a
    // direct source.
    for global in &project.globals {
        if !global.wado_mutable
            && let Some(n) = const_seq_len_a(global.initializer.body(), global.initializer.expr())
        {
            record_seq_len(
                &mut env,
                (global.module_source.clone(), global.name.clone()),
                n,
            );
        }
    }
    // The inline `GlobalVarSet(X, <const>)` body globalization emits, read from
    // each function's arena body.
    let mut collector = SeqLenCollector {
        immutable: &immutable,
        env: &mut env,
    };
    for func_rc in &project.functions {
        if let Some(body) = func_rc.borrow().body.as_ref() {
            collector.visit_body(body);
        }
    }
    env
}

fn record_seq_len(env: &mut GlobalFieldEnv, key: GlobalKey, n: i32) {
    env.entry(key).or_default().insert(
        SeqField::Len.field_name().to_string(),
        Value::Int {
            value: i64::from(n) as u64,
            prim: PrimitiveType::I32,
        },
    );
}

/// Records the [`SeqField::Len`] of each immutable global assigned an inline
/// constant sequence via `GlobalVarSet`. Walks the arena body directly.
struct SeqLenCollector<'a> {
    immutable: &'a IndexSet<GlobalKey>,
    env: &'a mut GlobalFieldEnv,
}

impl SeqLenCollector<'_> {
    fn visit_body(&mut self, body: &Body) {
        self.visit_node(body, NodeRef::Block(body.root));
    }

    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(e) = node
            && let ExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } = &body.exprs[e].kind
        {
            let key = (module_source.clone(), name.clone());
            let value = *value;
            if self.immutable.contains(&key)
                && let Some(n) = const_seq_len_operand_a(body, value)
            {
                record_seq_len(self.env, key, n);
            }
        }
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            self.visit_node(body, c);
        }
    }
}

struct ConstFoldVisitor<'a> {
    interpreter: Interpreter<'a>,
}

/// The control-flow / scope expression shapes walked per-arm. Extracting them
/// up front (cloning the arm lists) releases the body borrow so the per-arm
/// walk can mutate the arena.
enum ExprShape {
    If(Operand, BlockId, Option<BlockId>),
    Match(Operand, Vec<ArmData>),
    Switch(Operand, Vec<BlockId>, BlockId),
    Block(BlockId),
    Labeled(BlockId, String),
    None,
}

fn expr_shape(body: &Body, e: ExprId) -> ExprShape {
    match &body.exprs[e].kind {
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => ExprShape::If(*condition, *then_branch, *else_branch),
        ExprKind::Match { expr, arms } => ExprShape::Match(*expr, arms.clone()),
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => ExprShape::Switch(*scrutinee, arms.clone(), *default),
        ExprKind::Block(b) => ExprShape::Block(*b),
        ExprKind::LabeledBlock { block, label, .. } => ExprShape::Labeled(*block, label.clone()),
        _ => ExprShape::None,
    }
}

impl ConstFoldVisitor<'_> {
    fn visit_block(&mut self, engine: &mut Engine, block: BlockId) -> bool {
        // Bottom-up: walk children first so each If stmt's condition is
        // already folded to a literal (when feasible) by the time we
        // ask the interpreter to splice the chosen branch into this block.
        let stmts = engine.body.blocks[block].stmts.clone();
        let mut changed = false;
        for s in stmts {
            changed |= self.visit_stmt(engine, s);
        }
        changed |= self.interpreter.reduce_local_block_via(
            &mut EngineSink {
                engine: &mut *engine,
            },
            block,
        );
        changed
    }

    fn visit_stmt(&mut self, engine: &mut Engine, s: StmtId) -> bool {
        // Control-flow stmts are walked per-arm. Locals need no branch fork:
        // the only mutation channel is `let mut`, recorded preemptively as
        // `NonConst`, so the single-walk env handling holds.
        let is_control = matches!(
            &engine.body.stmts[s].kind,
            StmtKind::Loop { .. } | StmtKind::LabeledBlock { .. } | StmtKind::If { .. }
        );
        if !is_control {
            // Bottom-up: walk children first so the RHS of `let x = …`
            // is already folded by the time we record `x` in env.
            let changed = self.walk_children(engine, NodeRef::Stmt(s));
            self.update_env_from_stmt(engine.body, s);
            return changed;
        }

        match &engine.body.stmts[s].kind {
            StmtKind::Loop { body: lb } => {
                // Loop back-edge: every local the body may reassign or mutably
                // borrow is dropped to `NonConst` before and after the body
                // walk (`apply_loop_invalidations`), a conservative fixpoint
                // approximation — only facts unaffected by the body hold at
                // entry and post-loop. Struct-field heap effects across the
                // loop are the engine `ValueGraph`'s concern, not niri's.
                let lb = *lb;
                let writes = collect_loop_write_effects(engine.body, lb);
                self.apply_loop_invalidations(&writes);
                self.visit_block(engine, lb)
            }
            StmtKind::LabeledBlock { block, .. } => {
                let block = *block;
                self.visit_block(engine, block)
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let condition = *condition;
                let then_block = *then_block;
                let else_block = *else_block;
                let mut changed = self.visit_operand(engine, condition);
                changed |= self.visit_block(engine, then_block);
                if let Some(eb) = else_block {
                    changed |= self.visit_block(engine, eb);
                }
                changed
            }
            _ => unreachable!("non-control-flow stmt reached control-flow arm"),
        }
    }

    fn visit_operand(&mut self, engine: &mut Engine, op: Operand) -> bool {
        op.as_expr().is_some_and(|e| self.visit_expr(engine, e))
    }

    fn visit_expr(&mut self, engine: &mut Engine, e: ExprId) -> bool {
        // `Assign { target, value }` is special-cased: the OUTER `target`
        // expression is an lvalue (write position) and niri's leaf
        // rewrites — particularly the `FieldAccess(Local, field)`
        // arm — would happily fold a known field-value into the LHS,
        // turning `obj.f = newval` into `5 = newval`. Only `target`'s
        // sub-expressions (the receiver of a `FieldAccess`, the
        // indexee of an `Index`) are read positions; walk those, but
        // leave the outer `target` shape opaque. After the walk,
        // observe what was assigned so the field env stays in sync.
        if matches!(&engine.body.exprs[e].kind, ExprKind::Assign { .. }) {
            return self.visit_assign(engine, e);
        }

        // Branch / scope expressions: walk the condition / scrutinee and each
        // arm, then reduce at this node. The local env needs no per-arm fork —
        // mutable locals are recorded `NonConst` up front — and field
        // reaching-def across arms is the engine `ValueGraph`'s concern.
        match expr_shape(engine.body, e) {
            ExprShape::If(condition, then_branch, else_branch) => {
                let mut changed = self.visit_operand(engine, condition);
                changed |= self.visit_block(engine, then_branch);
                if let Some(eb) = else_branch {
                    changed |= self.visit_block(engine, eb);
                }
                changed |= self.reduce_local(engine, e);
                changed
            }
            ExprShape::Match(scrutinee, arms) => {
                let mut changed = self.visit_operand(engine, scrutinee);
                for arm in &arms {
                    if let Some(g) = arm.guard {
                        changed |= self.visit_operand(engine, g);
                    }
                    changed |= self.visit_operand(engine, arm.body);
                }
                changed |= self.reduce_local(engine, e);
                changed
            }
            ExprShape::Switch(scrutinee, arms, default) => {
                let mut changed = self.visit_operand(engine, scrutinee);
                for arm in &arms {
                    changed |= self.visit_block(engine, *arm);
                }
                changed |= self.visit_block(engine, default);
                changed |= self.reduce_local(engine, e);
                changed
            }
            ExprShape::Block(b) => {
                let mut changed = self.visit_block(engine, b);
                changed |= self.reduce_local(engine, e);
                changed
            }
            ExprShape::Labeled(block, _label) => {
                let mut changed = self.visit_block(engine, block);
                changed |= self.reduce_local(engine, e);
                changed
            }
            ExprShape::None => {
                // Bottom-up walk for the remaining expressions.
                let mut changed = self.walk_children(engine, NodeRef::Expr(e));
                changed |= self.reduce_local(engine, e);
                changed
            }
        }
    }

    /// Commit a single-node niri rewrite at `e` through the engine.
    fn reduce_local(&mut self, engine: &mut Engine, e: ExprId) -> bool {
        self.interpreter.reduce_local_via(
            &mut EngineSink {
                engine: &mut *engine,
            },
            e,
        )
    }

    fn visit_pattern(&mut self, engine: &mut Engine, p: PatId) -> bool {
        // Patterns carry no foldable value of their own, but a
        // `ConstantValue { expr }` pattern wraps an expression that
        // must still be reduced. The generic walk routes that expr
        // back through `visit_expr`.
        self.walk_children(engine, NodeRef::Pat(p))
    }

    /// Recurse into every id-bearing child of `node`. The const-fold special
    /// cases (control flow, `Assign`) are handled by the callers before they
    /// reach this generic walk, so the only nodes routed here are the
    /// straight-line ones.
    fn walk_children(&mut self, engine: &mut Engine, node: NodeRef) -> bool {
        let mut kids = Vec::new();
        engine.body.for_each_child(node, |c| kids.push(c));
        let mut changed = false;
        for c in kids {
            changed |= match c {
                NodeRef::Stmt(s) => self.visit_stmt(engine, s),
                NodeRef::Expr(ex) => self.visit_expr(engine, ex),
                NodeRef::Block(b) => self.visit_block(engine, b),
                NodeRef::Pat(p) => self.visit_pattern(engine, p),
            };
        }
        changed
    }

    /// Walk an `Assign { target, value }` expression. The outer
    /// `target` shape is left opaque (lvalue); only its inner
    /// sub-expression is folded. After the walk, a bare `local = …`
    /// reassignment drops `local`'s lattice to unknown (field / heap
    /// writes are the engine `ValueGraph`'s concern, not niri's).
    fn visit_assign(&mut self, engine: &mut Engine, e: ExprId) -> bool {
        let (target, value) = match &engine.body.exprs[e].kind {
            ExprKind::Assign { target, value } => (*target, *value),
            _ => unreachable!("visit_assign called on non-Assign"),
        };
        let mut changed = self.visit_operand(engine, value);
        let inner_to_walk = match &engine.body.exprs[target].kind {
            ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Index { expr: inner, .. } => {
                Some(*inner)
            }
            _ => None,
        };
        if let Some(inner) = inner_to_walk {
            changed |= self.visit_operand(engine, inner);
        }
        // A bare `local = …` reassignment drops the local's lattice to
        // unknown. A field / deref / index store needs nothing here — the
        // engine `ValueGraph` models those heap writes.
        if let ExprKind::Local { index, .. } = &engine.body.exprs[target].kind {
            self.interpreter.invalidate_local(*index);
        }
        changed |= self.reduce_local(engine, e);
        changed
    }

    /// After a statement is walked, capture any introduced binding into
    /// the interpreter's env so subsequent uses can fold against it.
    fn update_env_from_stmt(&mut self, body: &Body, s: StmtId) {
        let (local_index, is_mut, value) = match &body.stmts[s].kind {
            StmtKind::Let {
                local_index,
                is_mut,
                value,
                ..
            } => (*local_index, *is_mut, *value),
            _ => return,
        };
        let lat = if is_mut {
            // `let mut x = …` — any later `x = …` would invalidate the
            // binding anyway. The interpreter doesn't track
            // flow-sensitive values for mutable locals, so be
            // conservative up front.
            Lattice::NonConst
        } else {
            match value {
                Operand::Expr(e) => self.interpreter.reduce_to_lattice_a(body, e),
                Operand::Value(_) => self.interpreter.operand_to_lattice_a(body, value),
            }
        };
        // Drop any prior knowledge keyed by this index (rare — a fresh
        // `let` typically introduces a unique index, but defensive).
        // This also clears stale field entries from a same-index reuse
        // before we record new ones below.
        self.interpreter.invalidate_local(local_index);
        self.interpreter.bind_local(local_index, lat);
    }

    /// Apply a [`LoopWriteEffects`] summary to the interpreter,
    /// dropping every local the body could reassign or mutably borrow to
    /// `NonConst`, so the pre-body and post-body local env is a sound
    /// abstraction of any iteration count.
    fn apply_loop_invalidations(&mut self, writes: &LoopWriteEffects) {
        for idx in &writes.reassigned_locals {
            self.interpreter.invalidate_local(*idx);
        }
        for idx in &writes.mut_borrowed {
            // A `&mut local` (or `is_mut` call arg) escapes a mutable
            // reference the callee can store and mutate; drop the local.
            self.interpreter.invalidate_local(*idx);
        }
    }
}

/// Summary of every local a loop body could mutate. Used by
/// [`ConstFoldVisitor::apply_loop_invalidations`] to drop just those
/// `local` lattice entries before and after the body walk — facts about
/// locals the body does not touch survive.
#[derive(Default)]
struct LoopWriteEffects {
    /// `local = expr` targets — fully reassigned, so the local's lattice
    /// must drop to unknown across the loop.
    reassigned_locals: IndexSet<u32>,
    /// `&mut local` or `is_mut` call argument — callee may store and
    /// mutate through the reference, so drop the local fully.
    mut_borrowed: IndexSet<u32>,
}

/// Walk a loop body and collect every write effect that must be
/// invalidated before and after the walk. See [`LoopWriteEffects`].
fn collect_loop_write_effects(body: &Body, block: BlockId) -> LoopWriteEffects {
    let mut effects = LoopWriteEffects::default();
    collect_loop_writes(body, NodeRef::Block(block), &mut effects);
    effects
}

/// Record the write effects of `node`, then recurse into every child —
/// nested blocks and loops included, so the whole subtree is scanned.
fn collect_loop_writes(body: &Body, node: NodeRef, effects: &mut LoopWriteEffects) {
    if let NodeRef::Expr(e) = node {
        record_loop_write(body, e, effects);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_loop_writes(body, c, effects);
    }
}

fn record_loop_write(body: &Body, e: ExprId, effects: &mut LoopWriteEffects) {
    match &body.exprs[e].kind {
        // `local = …` reassigns the local across iterations.
        ExprKind::Assign { target, .. } => {
            if let ExprKind::Local { index, .. } = &body.exprs[*target].kind {
                effects.reassigned_locals.insert(*index);
            }
        }
        // `&mut local` escapes a mutable reference the callee can store and
        // write through. (`&mut local.field` mutates only the field, not the
        // local's binding, so it needs no local-lattice invalidation.)
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let Some(ie) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
            {
                effects.mut_borrowed.insert(*index);
            }
        }
        // A mut-ref argument or a `&mut self` method receiver may be mutated.
        ExprKind::Call { args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let Some(ae) = arg.expr.as_expr()
                    && let ExprKind::Local { index, .. } = &body.exprs[ae].kind
                {
                    effects.mut_borrowed.insert(*index);
                }
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            if let Some(re) = receiver.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[re].kind
            {
                effects.mut_borrowed.insert(*index);
            }
            for arg in args {
                if arg.is_mut
                    && let Some(ae) = arg.expr.as_expr()
                    && let ExprKind::Local { index, .. } = &body.exprs[ae].kind
                {
                    effects.mut_borrowed.insert(*index);
                }
            }
        }
        _ => {}
    }
}
