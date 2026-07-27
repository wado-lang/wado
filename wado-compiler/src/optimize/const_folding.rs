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

use super::gate::{FunctionGate, GatedPass};
use crate::compiler_item::SeqField;
use crate::const_eval::Value;
use crate::hashmap::IndexSet;
use crate::nir::NirFunction;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::niri::{
    CalleeMap, EditSink, GlobalEnv, GlobalFieldEnv, GlobalKey, Interpreter, Lattice, SeqBuiltin,
    SeqBuiltinMap, is_ctfe_eligible,
};
use crate::tir::{PrimitiveType, TypeId, TypeTable};

/// The three whole-program maps [`fold_constants`] feeds its interpreter. Each
/// is a fresh-per-build allocation ([`build_callee_map`] walks every function,
/// [`build_global_field_env`] every global *and* every function body), so
/// rebuilding them on every fixed-point iteration is pure overhead when nothing
/// they depend on changed. [`ConstFoldCache`] reuses them across iterations.
struct FoldMaps {
    callees: CalleeMap,
    seq_builtins: SeqBuiltinMap,
    globals: GlobalEnv,
    global_fields: GlobalFieldEnv,
}

fn build_fold_maps(project: &NirPackage, type_table: &TypeTable) -> FoldMaps {
    // The CalleeMap holds Rc handles aliased with `project.functions`. The
    // interpreter reads callee bodies via `try_borrow`, which bails cleanly when
    // the visitor already holds `borrow_mut` on the same function (a self-call
    // inside the function being walked). Because the Rc points at the live cell,
    // a callee's body edit is visible without rebuilding the map — only its
    // *membership* (the ctfe-eligible function set) can go stale.
    let callees = build_callee_map(project);
    let seq_builtins = build_seq_builtin_map(project);
    // Every immutable global whose initializer reduces to a `Const(_)` becomes a
    // `GlobalVarGet` rewrite target; mutable globals are recorded as `NonConst`.
    let globals = build_global_env(project, type_table, &callees);
    // Known constant fields of immutable globals — the `SeqField::Len` length of
    // sequence globals, so a `global:X.used` read folds and the bounds-check /
    // branch passes can drop the checks they eliminate on the pre-hoist local.
    let global_fields = build_global_field_env(project);
    FoldMaps {
        callees,
        seq_builtins,
        globals,
        global_fields,
    }
}

/// Cross-iteration cache for [`FoldMaps`], owned by the fixed-point loop and
/// threaded into each gated [`fold_constants`] call.
///
/// The maps depend only on the set of functions and globals, not on function
/// *body* content: the [`CalleeMap`]'s `Rc` handles track body edits
/// automatically, the [`GlobalEnv`] reads only global initializers (untouched by
/// the function-scoped loop passes), and the [`GlobalFieldEnv`]'s body scan finds
/// inline `GlobalVarSet`s to immutable globals — a shape only
/// `const_object_globalization` (a post-loop
/// pass) produces, so it contributes nothing during the loop. Hence the cache is
/// valid while the function and global counts are unchanged; `value_copy_demote`
/// appending a specialization (function count grows) or DCE around the loop
/// (global count changes) invalidates it, forcing a rebuild.
pub(super) struct ConstFoldCache {
    funcs_len: usize,
    globals_len: usize,
    maps: FoldMaps,
}

/// Apply constant folding to all functions in the project.
/// Flow-sensitive constant folding, gated: skips functions unchanged since this
/// pass last ran. Used in the fixed-point loop, reusing `cache`'s [`FoldMaps`]
/// unless the function/global counts changed since they were built.
pub fn fold_constants(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    cache: &mut Option<ConstFoldCache>,
) -> bool {
    let type_table = project.type_table.borrow();
    let funcs_len = project.functions.len();
    let globals_len = project.globals.len();
    let stale = cache
        .as_ref()
        .is_none_or(|c| c.funcs_len != funcs_len || c.globals_len != globals_len);
    if stale {
        *cache = Some(ConstFoldCache {
            funcs_len,
            globals_len,
            maps: build_fold_maps(project, &type_table),
        });
    }
    let maps = &cache.as_ref().expect("just populated").maps;
    let mut visitor = new_visitor(&type_table, maps);
    let mut buffers = EngineBuffers::default();
    let len = project.functions.len();
    gate.run_gated(GatedPass::ConstFold, len, |fid| {
        fold_function(&project.functions[fid.index()], &mut visitor, &mut buffers)
    })
}

/// Ungated variant: folds every function, rebuilding the maps each call. Used by
/// the post-globalization cleanup, whose bodies carry the inline `GlobalVarSet`s
/// globalization emits — so its [`GlobalFieldEnv`] is body-dependent and must not
/// be cached across the caller's fixed point.
pub fn fold_constants_all(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let maps = build_fold_maps(project, &type_table);
    let mut visitor = new_visitor(&type_table, &maps);
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        changed |= fold_function(func_rc, &mut visitor, &mut buffers);
    }
    changed
}

fn new_visitor<'a>(type_table: &'a TypeTable, maps: &'a FoldMaps) -> ConstFoldVisitor<'a> {
    let mut visitor = ConstFoldVisitor {
        interpreter: Interpreter::new(type_table),
    };
    visitor.interpreter.with_callees(&maps.callees);
    visitor.interpreter.with_seq_builtins(&maps.seq_builtins);
    visitor.interpreter.with_globals(&maps.globals);
    visitor.interpreter.with_global_fields(&maps.global_fields);
    visitor
}

fn fold_function(
    func_rc: &RefCell<NirFunction>,
    visitor: &mut ConstFoldVisitor<'_>,
    buffers: &mut EngineBuffers,
) -> bool {
    let mut func = func_rc.borrow_mut();
    let NirFunction { body, locals, .. } = &mut *func;
    let Some(body) = body.as_mut() else {
        return false;
    };
    // Local indices are per-function; reset the interpreter env at each boundary.
    visitor.interpreter.enter_function();
    visitor.interpreter.record_ref_global_aliases(body);
    visitor.interpreter.record_aggregate_locals(body);
    let mut engine = Engine::new(body, buffers, locals);
    let root = engine.body.root;
    visitor.visit_block(&mut engine, root)
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
        // The Live ValueGraph). A node with no operand parent slot (e.g. a body
        // root) cannot be promoted; report no change so the worklist settles.
        engine.replace_expr_with_value(id, value)
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
        let Some(id) = func.id else {
            continue;
        };
        drop(func);
        map.insert(id, func_rc.clone());
    }
    map
}

/// Which callee ids are the array builtins the engine evaluates.
fn build_seq_builtin_map(project: &NirPackage) -> SeqBuiltinMap {
    let mut map = SeqBuiltinMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(id) = func.id else {
            continue;
        };
        let descriptor = crate::nir::FunctionRef::from_resolved(&func, func.module_source.clone());
        let Some(name) = descriptor.monomorphized_builtin_name() else {
            continue;
        };
        let builtin = match name.as_str() {
            "builtin::array_get" => SeqBuiltin::Get,
            "builtin::array_len" => SeqBuiltin::Len,
            _ => continue,
        };
        map.insert(id, builtin);
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
        let declared = (!global.wado_mutable)
            .then(|| global.init.declared())
            .flatten();
        let lattice = match declared {
            None => Lattice::NonConst,
            Some(declared) => {
                let mut interp = Interpreter::new(type_table);
                interp.with_callees(callees);
                interp.with_globals(&env);
                let body = declared.body();
                match declared.expr() {
                    Operand::Expr(e) => interp.reduce_to_lattice_a(body, e),
                    op @ Operand::Value(_) => interp.operand_to_lattice_a(body, op),
                }
            }
        };
        if !matches!(lattice, Lattice::Unevaluated) {
            env.insert(key, lattice);
        }
    }
    env
}

/// The integer value of an operand — a promoted `ValueKind::Int` in the pool.
fn operand_int_a(body: &Body, op: Operand) -> Option<u64> {
    body.operand_const_int(op)
}

fn const_seq_len_operand_a(body: &Body, op: Operand) -> Option<i32> {
    op.as_expr().and_then(|e| const_seq_len_a(body, e))
}

fn tail_local_a(body: &Body, op: Operand) -> Option<u32> {
    match &body.exprs[op.as_expr()?].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            tail_local_a(body, *inner)
        }
        _ => None,
    }
}

/// Arena counterpart of [`const_seq_len`]: the statically-known
/// [`SeqField::Len`] length of a constant `List` / `String` value held
/// in the arena. Used by [`SeqLenCollector`] to read the value of an
/// inline `GlobalVarSet(X, <const>)` directly from the function's arena
/// body.
fn const_seq_len_a(body: &Body, e: ExprId) -> Option<i32> {
    match &body.exprs[e].kind {
        ExprKind::ArrayLiteral { elements } => i32::try_from(elements.len()).ok(),
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            let stmts = &body.blocks[*b].stmts;
            let (&last, rest) = stmts.split_last()?;
            if rest
                .iter()
                .any(|&s| !matches!(body.stmts[s].kind, StmtKind::Let { .. }))
            {
                return None;
            }
            let tail = match &body.stmts[last].kind {
                StmtKind::Expr(ex) => *ex,
                StmtKind::Break { value: Some(v), .. } => *v,
                _ => return None,
            };
            if let Some(len) = const_seq_len_operand_a(body, tail) {
                return Some(len);
            }
            let index = tail_local_a(body, tail)?;
            // Stop at the nearest (last) `let` of `index`: it shadows any earlier
            // binding. If that binding is non-const, the length is unknown —
            // scanning past it to an earlier const `let` would return a stale
            // length for a value the nearest binding already replaced.
            let nearest = rest.iter().rev().find(|&&s| {
                matches!(&body.stmts[s].kind, StmtKind::Let { local_index, .. } if *local_index == index)
            })?;
            let StmtKind::Let { value, .. } = &body.stmts[*nearest].kind else {
                unreachable!("`nearest` matched a `let` of `index` above")
            };
            const_seq_len_operand_a(body, *value)
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
            && let Some(declared) = global.init.declared()
            && let Some(init_e) = declared.expr().as_expr()
            && let Some(n) = const_seq_len_a(declared.body(), init_e)
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
                    // Under a constant scrutinee the arm's guard and body reduce
                    // under the bindings its pattern makes.
                    let binds = self
                        .interpreter
                        .arm_bindings(engine.body, scrutinee, arm.pattern);
                    let scope = self.interpreter.enter_arm(&binds);
                    if let Some(g) = arm.guard {
                        changed |= self.visit_operand(engine, g);
                    }
                    changed |= self.visit_operand(engine, arm.body);
                    self.interpreter.leave_arm(scope);
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
                changed |= self.project_struct_literal(engine, e);
                changed |= self.reduce_local(engine, e);
                changed
            }
        }
    }

    /// Fold `Struct { f: v, .. }.f` into `v` when the construction is
    /// immediate (the `FieldAccess` receiver is the literal itself) and every
    /// non-projected field is a pooled value, so dropping the struct discards
    /// no side effect. After copy-prop substitutes a pure single-field box
    /// literal into its sole `.field` use this is the fold that removes the
    /// otherwise-dead `struct.new` (issue: operand-promotion missed-opt).
    fn project_struct_literal(&mut self, engine: &mut Engine, e: ExprId) -> bool {
        let ExprKind::FieldAccess {
            expr: recv,
            field_name,
            ..
        } = &engine.body.exprs[e].kind
        else {
            return false;
        };
        let (recv, field_name) = (*recv, field_name.clone());
        let Some(recv_e) = recv.as_expr() else {
            return false;
        };
        // Split the projected field from its siblings within the struct-literal
        // borrow. `Operand` is `Copy`, so collecting the siblings carries no
        // `String` clone; the names themselves are only needed for the equality
        // test here, not afterwards.
        let mut projected = None;
        let siblings: Vec<Operand> = {
            let ExprKind::StructLiteral { fields, .. } = &engine.body.exprs[recv_e].kind else {
                return false;
            };
            let mut siblings = Vec::with_capacity(fields.len().saturating_sub(1));
            for f in fields {
                if f.name == field_name {
                    projected = Some(f.value);
                } else {
                    siblings.push(f.value);
                }
            }
            siblings
        };
        let Some(proj) = projected else {
            return false;
        };
        // A non-projected sibling with an observable effect must keep the struct
        // so its evaluation is preserved. A pure sibling (e.g. a `PackedArray`
        // repr) is dropped with the struct.
        if siblings
            .iter()
            .any(|op| !super::arena_query::is_pure_operand(engine.body, *op))
        {
            return false;
        }
        engine.redirect_expr(e, proj)
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
            // A destructure can rebind an index a prior `let` recorded — index
            // reuse is real (`labeled_block_fusion` producing a `LetDestructure`,
            // see `const_object_globalization`'s `locals_declared_once` note). The
            // interpreter tracks no lattice value for a destructured binding, so
            // drop the stale entry for every local the pattern binds; otherwise a
            // reused index keeps the earlier `let`'s constant.
            StmtKind::LetDestructure { pattern, .. } => {
                let mut stack = vec![NodeRef::Pat(*pattern)];
                while let Some(node) = stack.pop() {
                    if let NodeRef::Pat(p) = node
                        && let PatKind::Binding { local_index, .. } = &body.pats[p].kind
                    {
                        self.interpreter.invalidate_local(*local_index);
                    }
                    body.for_each_child(node, |c| stack.push(c));
                }
                return;
            }
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
