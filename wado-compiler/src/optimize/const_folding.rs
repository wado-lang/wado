//! Constant folding optimization for Wado NIR.
//!
//! Walks every function's arena [`Body`] and applies the
//! [`niri::Interpreter`] rewrite rules at each visited node. All
//! reduction logic (literal folding, integer cast collapsing,
//! short-circuit identity rules, env-aware local lookup, **field-aware
//! local-field reads**) lives in [`crate::niri`]; this module is only
//! the visitor glue that drives `reduce_local_a` across function bodies
//! and feeds the interpreter's local-variable env *and* its `field_env`
//! from `Let` / `Assign` statements, struct-literal RHSs, and recognized
//! `$value_copy$T(arg)` helpers.
//!
//! The visitor mutates the arena `Body` directly: the per-node rewrites
//! (`reduce_local_a`) and the block-level branch splice
//! (`reduce_local_block_a`) operate on arena ids, so const-fold no longer
//! round-trips the body through a tree bridge. Global initializers are arena
//! `ExprBody`s too, so the global env / field env are read from them via the
//! arena interpreter path.
//!
//! The field-knowledge bookkeeping was originally a separate pass
//! (`optimize::field_forward`); merging it into const-fold breaks the
//! per-iteration ping-pong observed at `-O3 inline_threshold ≥ 35`,
//! where a single iteration only propagates one statement of a chain
//! because `field_forward` and `const_fold` had to alternate to make
//! `let used = __b.used; __b.used = used + 1` advance one push at a
//! time. With both responsibilities in the same walk, the chain
//! folds in a single pass.
//!
//! See [`super::alias::build_alias_info`] /
//! [`super::alias::build_value_copy_helpers`] for the per-function
//! alias / helper computations the visitor consumes.

use std::cell::RefCell;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, FunctionId, GatedPass};
use crate::compiler_item::SeqField;
use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, PatId, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::niri::{
    Arm, CalleeMap, FieldSnapshot, GlobalEnv, GlobalFieldEnv, GlobalKey, Interpreter, Lattice,
    Value, is_ctfe_eligible,
};
use crate::tir::{PrimitiveType, TypeId, TypeTable};

use super::alias::{build_alias_info, build_value_copy_helpers, recognize_value_copy_a};
use super::arena_query::has_break_to;

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
    // Build the `$value_copy$T<id>` helpers map once per pass; the
    // visitor uses it to recognize calls that transfer field
    // knowledge across the synthesized one-level shallow copies
    // (see `lower::plan::value_copy::synthesize`).
    let value_copy_helpers = build_value_copy_helpers(project);
    let mut visitor = ConstFoldVisitor {
        interpreter: Interpreter::new(&type_table),
        type_table: &type_table,
        value_copy_helpers: &value_copy_helpers,
    };
    visitor.interpreter.with_callees(&callees);
    visitor.interpreter.with_globals(&globals);
    visitor.interpreter.with_global_fields(&global_fields);
    for (i, func_rc) in project.functions.iter().enumerate() {
        let fid = FunctionId::new(i);
        if let Some(g) = gate.as_deref_mut()
            && !g.needs(GatedPass::ConstFold, fid)
        {
            continue;
        }
        let mut func = func_rc.borrow_mut();
        let address_taken = func.address_taken_locals.clone();
        let stores_aliased = func.stores_aliased_locals.clone();
        let locals = func.locals.clone();
        let func_changed = if let Some(body) = func.body.as_mut() {
            // Local indices are unique per function, not project-wide,
            // so reset the interpreter's env at every function boundary.
            visitor.interpreter.enter_function();
            // Compute per-function alias annotations (driven by the
            // function's stable address-taken / stores sets plus a
            // body walk for transient inlined-in copies). The interpreter
            // consults these every time the visitor calls `bind_field` /
            // `invalidate_field` / `invalidate_aliased_fields`.
            let alias_info =
                build_alias_info(body, &locals, &address_taken, &stores_aliased, &type_table);
            visitor.interpreter.set_alias_info(alias_info);
            let root = body.root;
            visitor.visit_block(body, root)
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
/// program-wide [`CalleeMap`] is installed; the per-function `env` / `field_env`
/// stay empty, so the flow-sensitive folds (env-bound locals, forwarded fields,
/// immutable globals, constant-branch collapse) remain with the standalone
/// [`fold_constants`] walker that still runs once per fixed-point iteration.
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
        let Some(kind) = self
            .interpreter
            .borrow_mut()
            .const_fold_kind_a(engine.body, id)
        else {
            return false;
        };
        engine.replace_expr_kind(id, kind);
        true
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
                    StmtKind::Let { value, .. } => const_seq_len_a(body, *value),
                    StmtKind::Expr(ex) => const_seq_len_a(body, *ex),
                    _ => None,
                })
        }
        ExprKind::StructLiteral { fields, .. } => fields.iter().find_map(|f| {
            if f.name == SeqField::Len.field_name()
                && let ExprKind::IntLiteral { value, .. } = &body.exprs[f.value].kind
            {
                return i32::try_from(*value).ok();
            }
            None
        }),
        ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            const_seq_len_a(body, *inner)
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
                && let Some(n) = const_seq_len_a(body, value)
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
    type_table: &'a TypeTable,
    /// `(module_source, func_name) → struct type id` for every
    /// synthesized `$value_copy$T<id>` helper. The visitor uses this
    /// to recognize `Call(helper, [Local(src)])` shapes inside `let
    /// dst = …` and propagate `src`'s recorded fields onto `dst`
    /// via [`Self::update_field_env_from_let`].
    value_copy_helpers: &'a IndexMap<(ModuleSource, String), TypeId>,
}

/// The control-flow / scope expression shapes that need branch-aware
/// field-env handling. Extracting them up front (cloning the arm lists)
/// releases the body borrow so the per-arm walk can mutate the arena.
enum ExprShape {
    If(ExprId, BlockId, Option<BlockId>),
    Match(ExprId, Vec<ArmData>),
    Switch(ExprId, Vec<BlockId>, BlockId),
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
    fn visit_block(&mut self, body: &mut Body, block: BlockId) -> bool {
        // Bottom-up: walk children first so each If stmt's condition is
        // already folded to a literal (when feasible) by the time we
        // ask the interpreter to splice the chosen branch into this block.
        let stmts = body.blocks[block].stmts.clone();
        let mut changed = false;
        for s in stmts {
            changed |= self.visit_stmt(body, s);
        }
        changed |= self.interpreter.reduce_local_block_a(body, block);
        changed
    }

    fn visit_stmt(&mut self, body: &mut Body, s: StmtId) -> bool {
        // Control-flow stmts need branch-aware field-env handling so
        // a `local.field = …` inside a branch doesn't leak as known
        // field knowledge to code that runs only when the branch was
        // skipped. Locals don't need this fork (the only mutation
        // channel is `let mut`, recorded preemptively as `NonConst`),
        // so the existing single-walk env handling stays intact.
        let is_control = matches!(
            &body.stmts[s].kind,
            StmtKind::Loop { .. } | StmtKind::LabeledBlock { .. } | StmtKind::If { .. }
        );
        if !is_control {
            // Bottom-up: walk children first so the RHS of `let x = …`
            // is already folded by the time we record `x` in env /
            // field_env.
            let changed = self.walk_children(body, NodeRef::Stmt(s));
            self.update_env_from_stmt(body, s);
            return changed;
        }

        match &body.stmts[s].kind {
            StmtKind::Loop { body: lb } => {
                // Loop back-edge: compute the set of entities the body
                // could mutate, drop only those, then snapshot the
                // resulting (loop-invariant) field state. The body
                // walks against that state; on exit we restore from
                // the snapshot so:
                //
                // - Outer facts the body never touched survive,
                //   unlike the original blanket `clear_fields()` which
                //   wiped them.
                // - Bindings the body added mid-walk (and any
                //   `clear_fields()` an inner if-stmt invoked) do not
                //   leak past the loop boundary, since the snapshot is
                //   taken at the loop-invariant state.
                //
                // This is a correct conservative approximation of the
                // iteration fixpoint: at body entry and at post-loop,
                // only facts unaffected by the body hold.
                let lb = *lb;
                let writes = collect_loop_write_effects(body, lb);
                self.apply_loop_invalidations(&writes);
                let snap = self.interpreter.snapshot_fields();
                let changed = self.visit_block(body, lb);
                self.interpreter.restore_fields(snap);
                changed
            }
            StmtKind::LabeledBlock { block, label } => {
                // Sequential scope: outer knowledge flows in and the
                // body's bottom state flows out, the same way it would
                // for an unlabeled block. A `break label:` inside has
                // multiple-exit semantics that the const-fold engine
                // cannot precisely join in a single walk — fall back
                // to dropping field knowledge then. A trailing
                // `break label: value` is the inlined-function shape,
                // exits at the body bottom, and is fine to walk
                // straight through.
                let block = *block;
                let label = label.clone();
                let changed = self.visit_block(body, block);
                if has_non_tail_break_to(body, &label, block) {
                    self.interpreter.clear_fields();
                }
                changed
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let condition = *condition;
                let then_block = *then_block;
                let else_block = *else_block;
                let mut changed = self.visit_expr(body, condition);
                let snap_pre = self.interpreter.snapshot_fields();
                // Capture reachability BEFORE the walk: see
                // `block_falls_through`'s doc.
                let then_reachable = block_falls_through(body, then_block, self.type_table);
                changed |= self.visit_block(body, then_block);
                let snap_then = self.interpreter.snapshot_fields();
                self.interpreter.restore_fields(snap_pre.clone());
                let mut arms = vec![Arm {
                    reachable: then_reachable,
                    post_state: snap_then,
                }];
                if let Some(eb) = else_block {
                    let else_reachable = block_falls_through(body, eb, self.type_table);
                    changed |= self.visit_block(body, eb);
                    let snap_else = self.interpreter.snapshot_fields();
                    arms.push(Arm {
                        reachable: else_reachable,
                        post_state: snap_else,
                    });
                } else {
                    // Implicit else: reachable, no field writes.
                    arms.push(Arm {
                        reachable: true,
                        post_state: snap_pre.clone(),
                    });
                }
                let post_state = FieldSnapshot::join_arms(snap_pre, arms);
                self.interpreter.restore_fields(post_state);
                changed
            }
            _ => unreachable!("non-control-flow stmt reached control-flow arm"),
        }
    }

    fn visit_expr(&mut self, body: &mut Body, e: ExprId) -> bool {
        // `Assign { target, value }` is special-cased: the OUTER `target`
        // expression is an lvalue (write position) and niri's leaf
        // rewrites — particularly the `FieldAccess(Local, field)`
        // arm — would happily fold a known field-value into the LHS,
        // turning `obj.f = newval` into `5 = newval`. Only `target`'s
        // sub-expressions (the receiver of a `FieldAccess`, the
        // indexee of an `Index`) are read positions; walk those, but
        // leave the outer `target` shape opaque. After the walk,
        // observe what was assigned so the field env stays in sync.
        if matches!(&body.exprs[e].kind, ExprKind::Assign { .. }) {
            return self.visit_assign(body, e);
        }

        // Branch / scope expressions — fork or clear field state so
        // a `local.field = …` inside one arm doesn't leak as known
        // field knowledge to code reachable only when another arm
        // ran.
        match expr_shape(body, e) {
            ExprShape::If(condition, then_branch, else_branch) => {
                let mut changed = self.visit_expr(body, condition);
                let snap_pre = self.interpreter.snapshot_fields();
                let then_reachable = block_falls_through(body, then_branch, self.type_table);
                changed |= self.visit_block(body, then_branch);
                let snap_then = self.interpreter.snapshot_fields();
                self.interpreter.restore_fields(snap_pre.clone());
                let mut arms = vec![Arm {
                    reachable: then_reachable,
                    post_state: snap_then,
                }];
                if let Some(eb) = else_branch {
                    let else_reachable = block_falls_through(body, eb, self.type_table);
                    changed |= self.visit_block(body, eb);
                    let snap_else = self.interpreter.snapshot_fields();
                    arms.push(Arm {
                        reachable: else_reachable,
                        post_state: snap_else,
                    });
                } else {
                    arms.push(Arm {
                        reachable: true,
                        post_state: snap_pre.clone(),
                    });
                }
                let post_state = FieldSnapshot::join_arms(snap_pre, arms);
                self.interpreter.restore_fields(post_state);
                changed |= self.interpreter.reduce_local_a(body, e);
                changed
            }
            ExprShape::Match(scrutinee, arms) => {
                let mut changed = self.visit_expr(body, scrutinee);
                let snap_pre = self.interpreter.snapshot_fields();
                let mut arm_results: Vec<Arm> = Vec::with_capacity(arms.len());
                for arm in &arms {
                    let body_reachable =
                        !is_never_type(body.exprs[arm.body].type_id, self.type_table);
                    if let Some(g) = arm.guard {
                        changed |= self.visit_expr(body, g);
                    }
                    changed |= self.visit_expr(body, arm.body);
                    let snap_arm = self.interpreter.snapshot_fields();
                    self.interpreter.restore_fields(snap_pre.clone());
                    arm_results.push(Arm {
                        reachable: body_reachable,
                        post_state: snap_arm,
                    });
                }
                let post_state = FieldSnapshot::join_arms(snap_pre, arm_results);
                self.interpreter.restore_fields(post_state);
                changed |= self.interpreter.reduce_local_a(body, e);
                changed
            }
            ExprShape::Switch(scrutinee, arms, default) => {
                let mut changed = self.visit_expr(body, scrutinee);
                let snap_pre = self.interpreter.snapshot_fields();
                let mut arm_results: Vec<Arm> = Vec::with_capacity(arms.len() + 1);
                for arm in &arms {
                    let arm_reachable = block_falls_through(body, *arm, self.type_table);
                    changed |= self.visit_block(body, *arm);
                    let snap_arm = self.interpreter.snapshot_fields();
                    self.interpreter.restore_fields(snap_pre.clone());
                    arm_results.push(Arm {
                        reachable: arm_reachable,
                        post_state: snap_arm,
                    });
                }
                let default_reachable = block_falls_through(body, default, self.type_table);
                changed |= self.visit_block(body, default);
                let snap_default = self.interpreter.snapshot_fields();
                arm_results.push(Arm {
                    reachable: default_reachable,
                    post_state: snap_default,
                });
                let post_state = FieldSnapshot::join_arms(snap_pre, arm_results);
                self.interpreter.restore_fields(post_state);
                changed |= self.interpreter.reduce_local_a(body, e);
                changed
            }
            ExprShape::Block(b) => {
                // Unlabeled sequential scope: outer knowledge flows in
                // and the body's bottom state flows out. The block has
                // no label so no `break label:` can refer to it from
                // inside, and any invalidation the body performed
                // legitimately reflects post-block state.
                let mut changed = self.visit_block(body, b);
                changed |= self.interpreter.reduce_local_a(body, e);
                changed
            }
            ExprShape::Labeled(block, label) => {
                // Labeled block: same as the unlabeled case when the
                // only `break label:` (if any) is the trailing stmt
                // delivering the block's value — that's the shape the
                // inliner synthesises for an inlined function body, so
                // post-block state equals body bottom state.
                //
                // For richer break shapes (early `break label:` from
                // mid-body or from inside a nested expression) the
                // post-block state would be the join of break points
                // and the bottom — fall back to clearing rather than
                // computing the join.
                let mut changed = self.visit_block(body, block);
                if has_non_tail_break_to(body, &label, block) {
                    self.interpreter.clear_fields();
                }
                changed |= self.interpreter.reduce_local_a(body, e);
                changed
            }
            ExprShape::None => {
                // Bottom-up walk for the remaining expressions.
                let mut changed = self.walk_children(body, NodeRef::Expr(e));
                // After children have been walked, observe side effects
                // that could have mutated aliased state.
                self.update_field_env_from_expr(body, e);
                changed |= self.interpreter.reduce_local_a(body, e);
                changed
            }
        }
    }

    fn visit_pattern(&mut self, body: &mut Body, p: PatId) -> bool {
        // Patterns carry no foldable value of their own, but a
        // `ConstantValue { expr }` pattern wraps an expression that
        // must still be reduced. The generic walk routes that expr
        // back through `visit_expr`.
        self.walk_children(body, NodeRef::Pat(p))
    }

    /// Recurse into every id-bearing child of `node`. The const-fold special
    /// cases (control flow, `Assign`) are handled by the callers before they
    /// reach this generic walk, so the only nodes routed here are the
    /// straight-line ones.
    fn walk_children(&mut self, body: &mut Body, node: NodeRef) -> bool {
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        let mut changed = false;
        for c in kids {
            changed |= match c {
                NodeRef::Stmt(s) => self.visit_stmt(body, s),
                NodeRef::Expr(ex) => self.visit_expr(body, ex),
                NodeRef::Block(b) => self.visit_block(body, b),
                NodeRef::Pat(p) => self.visit_pattern(body, p),
            };
        }
        changed
    }

    /// Walk an `Assign { target, value }` expression. The outer
    /// `target` shape is left opaque (lvalue), only its inner
    /// sub-expression is folded. After the walk, the field env is
    /// updated from the assignment shape:
    ///
    /// - `local = expr`: invalidate `local` and the local-derived
    ///   field knowledge for it.
    /// - `local.field = lit`: invalidate `(local, field)` then re-bind
    ///   if `lit` is a forwardable literal.
    /// - `local.field = expr` where `expr` is non-literal: invalidate
    ///   `(local, field)`.
    /// - Any more complex target shape (`(*p).field = …`,
    ///   `arr[i] = …`, etc.): conservatively invalidate every
    ///   aliased local's fields.
    fn visit_assign(&mut self, body: &mut Body, e: ExprId) -> bool {
        let (target, value) = match &body.exprs[e].kind {
            ExprKind::Assign { target, value } => (*target, *value),
            _ => unreachable!("visit_assign called on non-Assign"),
        };
        let mut changed = self.visit_expr(body, value);
        let inner_to_walk = match &body.exprs[target].kind {
            ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Index { expr: inner, .. } => {
                Some(*inner)
            }
            _ => None,
        };
        if let Some(inner) = inner_to_walk {
            changed |= self.visit_expr(body, inner);
        }
        // Field-env update based on the (post-walk) target shape.
        let tgt = match &body.exprs[target].kind {
            ExprKind::Local { index, .. } => AssignTarget::Local(*index),
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => match &body.exprs[*inner].kind {
                ExprKind::Local { index, .. } => {
                    AssignTarget::FieldOnLocal(*index, field_name.clone())
                }
                _ => AssignTarget::Opaque,
            },
            _ => AssignTarget::Opaque,
        };
        match tgt {
            AssignTarget::Local(index) => {
                // Invalidates the local's lattice AND drops any field
                // knowledge tied to it. Captures field-knowledge
                // transfer for `dst = src` (Local→Local copy on a ref
                // type, where both names alias the same heap object).
                self.interpreter.invalidate_local(index);
                self.update_field_env_from_let(body, index, value);
            }
            AssignTarget::FieldOnLocal(local_index, field_name) => {
                self.interpreter.invalidate_field(local_index, &field_name);
                if let Some(v) = Value::from_arena_literal(body, value, self.type_table) {
                    self.interpreter.bind_field(local_index, &field_name, v);
                }
            }
            AssignTarget::Opaque => {
                // `(*p).field = …` / `q.outer.inner = …` / Index /
                // Deref — unknown receiver, drop every aliased local's
                // fields.
                self.interpreter.invalidate_aliased_fields();
            }
        }
        changed |= self.interpreter.reduce_local_a(body, e);
        changed
    }

    /// After a non-Assign expression's children have been walked,
    /// update the field env to reflect side-effects that may have
    /// mutated aliased state. Calls drop every aliased local's
    /// fields; `&mut local` escapes a mutable reference and drops
    /// `local`'s entry; struct / tuple / variant constructors that
    /// capture an aliased local invalidate aliased fields too.
    fn update_field_env_from_expr(&mut self, body: &Body, e: ExprId) {
        match &body.exprs[e].kind {
            ExprKind::Call { args, func, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                    {
                        self.interpreter.invalidate_local(*index);
                    }
                }
                // `$value_copy$T<id>(arg)` is a pure shallow copy that
                // doesn't mutate `arg`; the caller (visit_assign /
                // update_field_env_from_let) wants to copy field
                // knowledge from `arg` to the binding's target. Skip
                // the aliased-invalidation here so that path keeps
                // working.
                let key = (func.module_source.clone(), func.name.clone());
                if !self.value_copy_helpers.contains_key(&key) {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref hides &mut self: the receiver may have
                // been mutated by the call.
                if let ExprKind::Local { index, .. } = &body.exprs[*receiver].kind {
                    self.interpreter.invalidate_local(*index);
                }
                for arg in args {
                    if arg.is_mut
                        && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                    {
                        self.interpreter.invalidate_local(*index);
                    }
                }
                self.interpreter.invalidate_aliased_fields();
            }
            ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => {
                // Indirect callee is unknown — closures may capture
                // and mutate any aliased local.
                self.interpreter.invalidate_aliased_fields();
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let ExprKind::Local { index, .. } = &body.exprs[*inner].kind {
                    self.interpreter.invalidate_local(*index);
                }
            }
            ExprKind::StructLiteral { fields, .. } => {
                let aliased = self.interpreter.aliased_locals();
                let captured = fields
                    .iter()
                    .any(|f| value_captures_aliased_local(body, f.value, aliased));
                if captured {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                let aliased = self.interpreter.aliased_locals();
                let captured = elements
                    .iter()
                    .any(|el| value_captures_aliased_local(body, *el, aliased));
                if captured {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            ExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    let p = *p;
                    let aliased = self.interpreter.aliased_locals();
                    let captured = value_captures_aliased_local(body, p, aliased);
                    if captured {
                        self.interpreter.invalidate_aliased_fields();
                    }
                }
            }
            _ => {}
        }
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
            self.interpreter.reduce_to_lattice_a(body, value)
        };
        // Drop any prior knowledge keyed by this index (rare — a fresh
        // `let` typically introduces a unique index, but defensive).
        // This also clears stale field entries from a same-index reuse
        // before we record new ones below.
        self.interpreter.invalidate_local(local_index);
        self.interpreter.bind_local(local_index, lat);
        self.update_field_env_from_let(body, local_index, value);
    }

    /// Update field env after `let local = value` (or
    /// `local = value` Assign). Recognized RHS shapes:
    ///
    /// - `StructLiteral { f: lit, … }`: bind each forwardable
    ///   field into `field_env`.
    /// - `Local(src)`: copy `src`'s recorded fields onto `local`
    ///   (covers reference-typed `let dst = src` aliasing — for
    ///   value-typed copies the lower phase wraps in
    ///   `$value_copy$T(src)` so the next case handles them).
    /// - `Call($value_copy$T(src))`: same as above; the helper is a
    ///   one-level shallow copy (field-by-field projection plus
    ///   `array_clone` for raw arrays — see
    ///   `lower::plan::value_copy::synthesize`), and the only fields we
    ///   actually forward are primitive literals (`Int` / `Float` /
    ///   `Bool` / `Char`) for which a shallow copy is observably
    ///   equivalent to a deep copy. Reference-typed fields stay
    ///   un-forwarded so the shared backing they preserve doesn't
    ///   become a soundness hazard.
    /// - `Block { …; tail_expr }` / `LabeledBlock { …; break label: tail }`:
    ///   recurse on the producing tail. This is the shape produced by
    ///   inlining a constructor (`List::filled(16, 0)` becomes
    ///   `__inline_…: { …; break __inline_…: List<u16> { repr, used: 16 } }`),
    ///   so the constructor's field knowledge reaches the let target.
    fn update_field_env_from_let(&mut self, body: &Body, local_index: u32, value: ExprId) {
        // Unwrap a chained `$value_copy$T<id>(arg)` so the underlying
        // source's knowledge is what we read.
        let inner = recognize_value_copy_a(body, value, self.value_copy_helpers).unwrap_or(value);
        // Peer through Block / LabeledBlock tails: the producing tail
        // is the final expression of an unlabeled block, or the value
        // of the sole `break label: value` of a labeled block.
        if let Some(tail) = single_producing_tail(body, inner) {
            self.update_field_env_from_let(body, local_index, tail);
            return;
        }
        match &body.exprs[inner].kind {
            ExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    if let Some(v) = Value::from_arena_literal(body, f.value, self.type_table) {
                        self.interpreter.bind_field(local_index, &f.name, v);
                    }
                }
            }
            ExprKind::Local { index: src, .. } => {
                self.interpreter.copy_fields_from(*src, local_index);
            }
            _ => {}
        }
    }

    /// Apply a [`LoopWriteEffects`] summary to the interpreter,
    /// invalidating every local / field the body could mutate so the
    /// pre-body and post-body state reflect a sound abstraction of any
    /// possible iteration count.
    fn apply_loop_invalidations(&mut self, writes: &LoopWriteEffects) {
        for idx in &writes.reassigned_locals {
            self.interpreter.invalidate_local(*idx);
        }
        for idx in &writes.mut_borrowed {
            // A `&mut local` (or `is_mut` call arg) escapes a mutable
            // reference the callee can store and mutate; drop both
            // the local's lattice and any field knowledge.
            self.interpreter.invalidate_local(*idx);
        }
        for (idx, field) in &writes.written_fields {
            self.interpreter.invalidate_field(*idx, field);
        }
        if writes.has_external_writes {
            // Calls / indirect writes inside the body could have
            // mutated any aliased local's fields. Drop them.
            self.interpreter.invalidate_aliased_fields();
        }
    }
}

/// Resolved shape of an `Assign` target for the field-env update.
enum AssignTarget {
    Local(u32),
    FieldOnLocal(u32, String),
    Opaque,
}

/// True when `block` contains a `break label:` (with or without a
/// value) other than a trailing `break label: value` as its very last
/// statement. The "trailing-only" shape is what `inline` synthesises
/// to deliver a function's return value, and from a field-env point
/// of view it exits at the body bottom — outer / body facts can flow
/// through it intact.
///
/// A non-tail break, by contrast, is an early exit whose carried
/// field state may differ from the bottom state. The const-fold
/// engine cannot precisely join multiple exit states in a single
/// walk, so the caller drops field knowledge when this returns
/// `true`.
fn has_non_tail_break_to(body: &Body, label: &str, block: BlockId) -> bool {
    if !has_break_to(body, NodeRef::Block(block), label) {
        return false;
    }
    let block_stmts = body.blocks[block].stmts.clone();
    let Some(&last) = block_stmts.last() else {
        return false;
    };
    let last_is_tail_break_to_self = matches!(
        &body.stmts[last].kind,
        StmtKind::Break {
            label: Some(brk_label),
            ..
        } if brk_label == label
    );
    if !last_is_tail_break_to_self {
        return true;
    }
    // The last stmt is `break label: ...` — any other break-to-label
    // anywhere else in the block (including inside the trailing
    // break's carried value) qualifies as non-tail.
    if block_stmts[..block_stmts.len() - 1]
        .iter()
        .any(|s| has_break_to(body, NodeRef::Stmt(*s), label))
    {
        return true;
    }
    if let StmtKind::Break {
        value: Some(value), ..
    } = &body.stmts[last].kind
    {
        return has_break_to(body, NodeRef::Expr(*value), label);
    }
    false
}

/// Return the single value-producing tail of `expr` when peering
/// through `Block` / `LabeledBlock` wrappers is safe — i.e. the wrapper
/// has exactly one value-producing exit and dropping the wrapper would
/// not change which value reaches the consumer.
///
/// Recognised shapes:
///
/// - `Block { stmts; tail_expr }`: the trailing `Expr(tail_expr)` stmt is
///   the only producer (an unlabeled block has no break target).
/// - `LabeledBlock { label, block }` whose only reference to `label` is
///   a trailing `break label: value` stmt: that `value` is the sole
///   producer. This is the shape `inline` synthesises for a function
///   call — the only break exits through the label with the function's
///   return value.
///
/// The walk is non-recursive at the call site; callers (currently only
/// [`ConstFoldVisitor::update_field_env_from_let`]) recurse explicitly so
/// they can also handle the `$value_copy$T(arg)` / `StructLiteral` /
/// `Local` shapes that may sit directly inside the wrapper.
fn single_producing_tail(body: &Body, e: ExprId) -> Option<ExprId> {
    match &body.exprs[e].kind {
        ExprKind::Block(b) => {
            let last = *body.blocks[*b].stmts.last()?;
            let StmtKind::Expr(tail) = &body.stmts[last].kind else {
                return None;
            };
            Some(*tail)
        }
        ExprKind::LabeledBlock { label, block, .. } => {
            let block_stmts = body.blocks[*block].stmts.clone();
            let last = *block_stmts.last()?;
            let StmtKind::Break {
                label: Some(brk_label),
                value: Some(value),
            } = &body.stmts[last].kind
            else {
                return None;
            };
            if brk_label != label {
                return None;
            }
            let value = *value;
            // The trailing break is the only producer iff no other
            // stmt in the block, and no sub-expression of the tail
            // value, breaks to the same label.
            if block_stmts[..block_stmts.len() - 1]
                .iter()
                .any(|s| has_break_to(body, NodeRef::Stmt(*s), label))
            {
                return None;
            }
            if has_break_to(body, NodeRef::Expr(value), label) {
                return None;
            }
            Some(value)
        }
        _ => None,
    }
}

/// True when control can reach the bottom of `block`.
///
/// Used by the if-handlers to detect a trapping arm
/// (`if cond { panic("…"); }`, `if cond { return … }`) so the
/// arm's field-env mutations are excluded from the post-if join.
///
/// Reads the last stmt only — see [`stmt_falls_through`] for the
/// per-kind decisions. An empty block falls through trivially;
/// `Loop` is reported as falling through (we don't analyse breaks).
///
/// Callers MUST invoke this BEFORE walking the block: the walker
/// can rewrite the trailing expression's `type_id` (e.g. via
/// `reduce_local` reconstructing a wrapper), making a post-walk
/// read of `is_never_type` unreliable.
fn block_falls_through(body: &Body, block: BlockId, type_table: &TypeTable) -> bool {
    let Some(&last) = body.blocks[block].stmts.last() else {
        return true;
    };
    stmt_falls_through(body, last, type_table)
}

fn stmt_falls_through(body: &Body, s: StmtId, type_table: &TypeTable) -> bool {
    match &body.stmts[s].kind {
        StmtKind::Return { .. } | StmtKind::Break { .. } | StmtKind::Continue => false,
        StmtKind::Expr(expr) => !is_never_type(body.exprs[*expr].type_id, type_table),
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            !is_never_type(body.exprs[*value].type_id, type_table)
        }
        StmtKind::LabeledBlock { block, .. } => block_falls_through(body, *block, type_table),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            // Falls through iff some reachable arm falls through.
            // No `else` ⇒ the implicit (empty) else falls through.
            let then_block = *then_block;
            let else_block = *else_block;
            block_falls_through(body, then_block, type_table)
                || else_block.is_none_or(|eb| block_falls_through(body, eb, type_table))
        }
        StmtKind::Loop { .. } => true,
    }
}

/// True when `type_id` resolves to [`ResolvedType::Never`] — the
/// uninhabited `!` type the elaborator gives expressions that
/// definitely do not produce a value (`panic(…)`, `unreachable()`,
/// `loop { }`, calls whose return type is `!`).
fn is_never_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    type_table.is_never(type_id)
}

/// True when a `Call`'s callee is a compiler builtin that cannot
/// mutate any user-level struct field tracked by `niri`'s `field_env`.
/// Every builtin in `core:builtin` and `wasm-asset` (whether emitted
/// directly or via monomorphization) operates below the struct layer:
/// `array_set` writes an array element, `memory_grow` resizes linear
/// memory, `store_u8` writes a memory cell, etc. None of these touch
/// the `(local, field) → Value` entries `field_env` records, so a
/// loop that contains only such calls has no `external` field-env
/// effects.
///
/// Used by [`collect_loop_write_effects`] to keep `has_external_writes`
/// cleared for builtin-only loops so a pre-loop binding like
/// `arr.used = 16` survives the loop's pre-walk
/// `invalidate_aliased_fields` when `arr` is aliased.
fn is_field_env_pure_call(func: &FunctionRef) -> bool {
    func.builtin_name().is_some() || func.monomorphized_builtin_name().is_some()
}

/// True when an expression appearing as a struct / tuple / variant
/// field value would hand the freshly-built aggregate access to an
/// already-aliased local. Mirrors the predicate the original
/// `field_forward` pass used inline; kept here so the const-fold
/// visitor doesn't reach back into `optimize::alias`'s private
/// module surface.
fn value_captures_aliased_local(body: &Body, e: ExprId, aliased: &crate::niri::LocalSet) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Unary { op, expr: inner } => {
            (matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && matches!(body.exprs[*inner].kind, ExprKind::Local { .. }))
                || value_captures_aliased_local(body, *inner, aliased)
        }
        ExprKind::Local { index, .. } => aliased.contains(*index),
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
            value_captures_aliased_local(body, *inner, aliased)
        }
        _ => false,
    }
}

/// Summary of every entity a loop body could mutate. Used by
/// [`ConstFoldVisitor::apply_loop_invalidations`] to drop just those
/// `(local, field)` and `local` lattice entries before and after the
/// body walk — facts about entities the body does not touch survive.
#[derive(Default)]
struct LoopWriteEffects {
    /// `local = expr` targets — fully reassigned, so both lattice and
    /// every recorded field of the local must be dropped.
    reassigned_locals: IndexSet<u32>,
    /// `local.field = expr` targets — drop just `(local, field)`.
    written_fields: IndexSet<(u32, String)>,
    /// `&mut local` or `is_mut` call argument — callee may store and
    /// mutate through the reference, so drop the local fully.
    mut_borrowed: IndexSet<u32>,
    /// Any expression that could mutate aliased state from outside the
    /// straight-line walk: `Call`, `MethodCall`, `IndirectCall`,
    /// `CmRawCall`, or an `Assign` with an opaque target shape
    /// (`(*p).f = …`, `arr[i] = …`). Triggers
    /// [`Interpreter::invalidate_aliased_fields`].
    has_external_writes: bool,
}

/// Walk a loop body and collect every write effect that must be
/// invalidated before and after the walk. See [`LoopWriteEffects`].
fn collect_loop_write_effects(body: &Body, block: BlockId) -> LoopWriteEffects {
    let mut effects = LoopWriteEffects::default();
    collect_loop_writes(body, NodeRef::Block(block), &mut effects);
    effects
}

/// Record the write effects of `node`, then recurse into every child
/// (nested blocks and loops included — the full subtree, matching the
/// original `NirRefVisitor`-based collector).
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
        ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
            ExprKind::Local { index, .. } => {
                effects.reassigned_locals.insert(*index);
            }
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => match &body.exprs[*inner].kind {
                ExprKind::Local { index, .. } => {
                    effects.written_fields.insert((*index, field_name.clone()));
                }
                _ => {
                    // `(*p).f = …` / `q.outer.inner = …` — opaque
                    // receiver; conservatively treat as an external
                    // write so aliased fields drop.
                    effects.has_external_writes = true;
                }
            },
            ExprKind::Index { .. } => {
                // `arr[i] = …` mutates an array element, not any entry
                // in `field_env` (which records `(local, fieldname) →
                // Value`, not element-level state). Mirror
                // `visit_assign`'s opaque-write treatment: mark
                // `has_external_writes` so an aliased-local
                // invalidation fires only when the receiver could be
                // reachable through an external alias.
                effects.has_external_writes = true;
            }
            _ => {
                // Deref / other lvalue shape.
                effects.has_external_writes = true;
            }
        },
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => match &body.exprs[*inner].kind {
            ExprKind::Local { index, .. } => {
                effects.mut_borrowed.insert(*index);
            }
            ExprKind::FieldAccess {
                expr: receiver,
                field_name,
                ..
            } => match &body.exprs[*receiver].kind {
                ExprKind::Local { index, .. } => {
                    // `&mut local.field` — callee can replace the
                    // field or mutate its interior, so the cached
                    // `(local, field)` entry is stale.
                    effects.written_fields.insert((*index, field_name.clone()));
                }
                _ => {
                    // `&mut local.f.g` or deeper — fall back to
                    // dropping the whole rooted local, since we do not
                    // track fields-of-fields.
                    if let Some(root) = root_local_of(body, *receiver) {
                        effects.mut_borrowed.insert(root);
                    } else {
                        effects.has_external_writes = true;
                    }
                }
            },
            _ => {
                // `&mut (*p).x`, `&mut arr[i]`, … — opaque receiver;
                // treat as an external write.
                effects.has_external_writes = true;
            }
        },
        ExprKind::Call { func, args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                {
                    effects.mut_borrowed.insert(*index);
                }
            }
            // Builtin intrinsics (`array_*`, `select`, `cold_path`,
            // `memory_*`, `store_*`, `load_*`,
            // `copy_value`, the i64-128 helpers, …) never mutate
            // user-level struct fields tracked by `field_env`:
            // `array_set` writes an array element, `memory_grow`
            // resizes linear memory, and so on. Skipping
            // `has_external_writes` for them lets a `.used` / `.repr`
            // binding made before the loop survive past the loop
            // boundary, even when the body reads / writes the
            // underlying buffer.
            if !is_field_env_pure_call(func) {
                effects.has_external_writes = true;
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            // Auto-ref hides `&mut self`: receiver may be mutated.
            if let ExprKind::Local { index, .. } = &body.exprs[*receiver].kind {
                effects.mut_borrowed.insert(*index);
            }
            for arg in args {
                if arg.is_mut
                    && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                {
                    effects.mut_borrowed.insert(*index);
                }
            }
            effects.has_external_writes = true;
        }
        ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => {
            effects.has_external_writes = true;
        }
        _ => {}
    }
}

/// Walk down a `local.f.g.…` field chain and return the rooted local
/// index, or `None` if the chain is rooted at something other than a
/// `Local` (e.g. `(*p).f`).
fn root_local_of(body: &Body, e: ExprId) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } => root_local_of(body, *inner),
        _ => None,
    }
}
