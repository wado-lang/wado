//! Constant folding optimization for Wado NIR.
//!
//! Walks every function body and applies the [`niri::Interpreter`]
//! rewrite rules at each visited node. All reduction logic
//! (literal folding, integer cast collapsing, short-circuit identity
//! rules, env-aware local lookup, **field-aware local-field reads**)
//! lives in [`crate::niri`]; this module is only the visitor glue
//! that drives `reduce_local` across function bodies and feeds the
//! interpreter's local-variable env *and* its `field_env` from
//! `Let` / `Assign` statements, struct-literal RHSs, and recognized
//! `$value_copy$T(arg)` helpers.
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

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{
    NirOptVisitor, NirRefVisitor, block_has_break_to, expr_has_break_to, opt_walk_block,
    opt_walk_expr, opt_walk_stmt, stmt_has_break_to,
};
use crate::niri::{
    Arm, CalleeMap, FieldSnapshot, GlobalEnv, Interpreter, Lattice, Value, is_ctfe_eligible,
};
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::alias::{build_alias_info, build_value_copy_helpers, recognize_value_copy};

/// Apply constant folding to all functions in the project.
pub fn fold_constants(project: &mut NirPackage) -> bool {
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
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let address_taken = func.address_taken_locals.clone();
        let stores_aliased = func.stores_aliased_locals.clone();
        let locals = func.locals.clone();
        if let Some(ref mut body) = func.body {
            // Local indices are unique per function, not project-wide,
            // so reset the interpreter's env at every function boundary.
            visitor.interpreter.enter_function();
            // Compute per-function alias annotations (driven by the
            // function's stable address-taken / stores sets plus a
            // body walk for transient inlined-in copies). The
            // interpreter consults these every time the visitor calls
            // `bind_field` / `invalidate_field` /
            // `invalidate_aliased_fields`.
            let alias_info =
                build_alias_info(body, &locals, &address_taken, &stores_aliased, &type_table);
            visitor.interpreter.set_alias_info(alias_info);
            changed |= visitor.visit_block(body);
        }
    }
    changed
}

/// Pre-build the [`CalleeMap`] from every CTFE-eligible function in
/// `project`. The map stores `Rc<RefCell<NirFunction>>` handles
/// aliased with `project.functions`, so rebuilding the map every
/// optimizer iteration costs only refcount bumps. The key shape
/// `(module_source, full_name)` mirrors what `try_call_fold`
/// synthesises from a `Call` node's `FunctionRef`.
fn build_callee_map(project: &NirPackage) -> CalleeMap {
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
            interp.reduce_to_lattice(&global.initializer)
        };
        if !matches!(lattice, Lattice::Unevaluated) {
            env.insert(key, lattice);
        }
    }
    env
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

impl NirOptVisitor for ConstFoldVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &mut NirStmt) -> bool {
        // Control-flow stmts need branch-aware field-env handling so
        // a `local.field = …` inside a branch doesn't leak as known
        // field knowledge to code that runs only when the branch was
        // skipped. Locals don't need this fork (the only mutation
        // channel is `let mut`, recorded preemptively as `NonConst`),
        // so the existing single-walk env handling stays intact.
        match &mut stmt.kind {
            NirStmtKind::Loop { body } => {
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
                let writes = collect_loop_write_effects(body);
                self.apply_loop_invalidations(&writes);
                let snap = self.interpreter.snapshot_fields();
                let changed = self.visit_block(body);
                self.interpreter.restore_fields(snap);
                return changed;
            }
            NirStmtKind::LabeledBlock { block, label } => {
                // Sequential scope: outer knowledge flows in and the
                // body's bottom state flows out, the same way it would
                // for an unlabeled block. A `break label:` inside has
                // multiple-exit semantics that the const-fold engine
                // cannot precisely join in a single walk — fall back
                // to dropping field knowledge then. A trailing
                // `break label: value` is the inlined-function shape,
                // exits at the body bottom, and is fine to walk
                // straight through.
                let changed = self.visit_block(block);
                if has_non_tail_break_to(label, block) {
                    self.interpreter.clear_fields();
                }
                return changed;
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut changed = self.visit_expr(condition);
                let snap_pre = self.interpreter.snapshot_fields();
                // Capture reachability BEFORE the walk: see
                // `block_falls_through`'s doc.
                let then_reachable = block_falls_through(then_block, self.type_table);
                changed |= self.visit_block(then_block);
                let snap_then = self.interpreter.snapshot_fields();
                self.interpreter.restore_fields(snap_pre.clone());
                let mut arms = vec![Arm {
                    reachable: then_reachable,
                    post_state: snap_then,
                }];
                if let Some(eb) = else_block {
                    let else_reachable = block_falls_through(eb, self.type_table);
                    changed |= self.visit_block(eb);
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
                return changed;
            }
            _ => {}
        }

        // Bottom-up: walk children first so the RHS of `let x = …` is
        // already folded by the time we record `x` in env / field_env.
        let changed = opt_walk_stmt(self, stmt);
        self.update_env_from_stmt(stmt);
        changed
    }

    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        // `Assign { target, value }` is special-cased: the OUTER `target`
        // expression is an lvalue (write position) and niri's leaf
        // rewrites — particularly the `FieldAccess(Local, field)`
        // arm — would happily fold a known field-value into the LHS,
        // turning `obj.f = newval` into `5 = newval`. Only `target`'s
        // sub-expressions (the receiver of a `FieldAccess`, the
        // indexee of an `Index`) are read positions; walk those, but
        // leave the outer `target` shape opaque. After the walk,
        // observe what was assigned so the field env stays in sync.
        if matches!(&expr.kind, NirExprKind::Assign { .. }) {
            return self.visit_assign(expr);
        }

        // Branch / scope expressions — fork or clear field state so
        // a `local.field = …` inside one arm doesn't leak as known
        // field knowledge to code reachable only when another arm
        // ran.
        match &mut expr.kind {
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut changed = self.visit_expr(condition);
                let snap_pre = self.interpreter.snapshot_fields();
                let then_reachable = block_falls_through(then_branch, self.type_table);
                changed |= self.visit_block(then_branch);
                let snap_then = self.interpreter.snapshot_fields();
                self.interpreter.restore_fields(snap_pre.clone());
                let mut arms = vec![Arm {
                    reachable: then_reachable,
                    post_state: snap_then,
                }];
                if let Some(eb) = else_branch {
                    let else_reachable = block_falls_through(eb, self.type_table);
                    changed |= self.visit_block(eb);
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
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            NirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let mut changed = self.visit_expr(scrutinee);
                let snap_pre = self.interpreter.snapshot_fields();
                let mut arm_results: Vec<Arm> = Vec::with_capacity(arms.len());
                for arm in arms {
                    let body_reachable = !is_never_type(arm.body.type_id, self.type_table);
                    if let Some(g) = &mut arm.guard {
                        changed |= self.visit_expr(g);
                    }
                    changed |= self.visit_expr(&mut arm.body);
                    let snap_arm = self.interpreter.snapshot_fields();
                    self.interpreter.restore_fields(snap_pre.clone());
                    arm_results.push(Arm {
                        reachable: body_reachable,
                        post_state: snap_arm,
                    });
                }
                let post_state = FieldSnapshot::join_arms(snap_pre, arm_results);
                self.interpreter.restore_fields(post_state);
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            NirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                let mut changed = self.visit_expr(scrutinee);
                let snap_pre = self.interpreter.snapshot_fields();
                let mut arm_results: Vec<Arm> = Vec::with_capacity(arms.len() + 1);
                for arm in arms.iter_mut() {
                    let arm_reachable = block_falls_through(arm, self.type_table);
                    changed |= self.visit_block(arm);
                    let snap_arm = self.interpreter.snapshot_fields();
                    self.interpreter.restore_fields(snap_pre.clone());
                    arm_results.push(Arm {
                        reachable: arm_reachable,
                        post_state: snap_arm,
                    });
                }
                let default_reachable = block_falls_through(default, self.type_table);
                changed |= self.visit_block(default);
                let snap_default = self.interpreter.snapshot_fields();
                arm_results.push(Arm {
                    reachable: default_reachable,
                    post_state: snap_default,
                });
                let post_state = FieldSnapshot::join_arms(snap_pre, arm_results);
                self.interpreter.restore_fields(post_state);
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            NirExprKind::Block(b) => {
                // Unlabeled sequential scope: outer knowledge flows in
                // and the body's bottom state flows out. The block has
                // no label so no `break label:` can refer to it from
                // inside, and any invalidation the body performed
                // legitimately reflects post-block state.
                let mut changed = self.visit_block(b);
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            NirExprKind::LabeledBlock { block, label, .. } => {
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
                let mut changed = self.visit_block(block);
                if has_non_tail_break_to(label, block) {
                    self.interpreter.clear_fields();
                }
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            _ => {}
        }

        // Bottom-up walk for the remaining expressions.
        let mut changed = opt_walk_expr(self, expr);
        // After children have been walked, observe side effects that
        // could have mutated aliased state.
        self.update_field_env_from_expr(expr);
        changed |= self.interpreter.reduce_local(expr);
        changed
    }

    fn visit_block(&mut self, block: &mut NirBlock) -> bool {
        // Bottom-up: walk children first so each If stmt's condition is
        // already folded to a literal (when feasible) by the time we
        // ask the interpreter to splice the chosen branch into this block.
        let mut changed = opt_walk_block(self, block);
        changed |= self.interpreter.reduce_local_block(block);
        changed
    }
}

impl ConstFoldVisitor<'_> {
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
    fn visit_assign(&mut self, expr: &mut NirExpr) -> bool {
        let NirExprKind::Assign { target, value } = &mut expr.kind else {
            unreachable!("visit_assign called on non-Assign");
        };
        let mut changed = self.visit_expr(value);
        match &mut target.kind {
            NirExprKind::FieldAccess { expr: inner, .. }
            | NirExprKind::Index { expr: inner, .. } => {
                changed |= self.visit_expr(inner);
            }
            _ => {}
        }
        // Field-env update based on the (post-walk) shape.
        match &target.kind {
            NirExprKind::Local { index, .. } => {
                // Invalidates the local's lattice AND drops any field
                // knowledge tied to it. Captures field-knowledge
                // transfer for `dst = src` (Local→Local copy on a ref
                // type, where both names alias the same heap object).
                self.interpreter.invalidate_local(*index);
                let dst = *index;
                self.update_field_env_from_let(dst, value);
            }
            NirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => match &inner.kind {
                NirExprKind::Local { index, .. } => {
                    let local_index = *index;
                    self.interpreter.invalidate_field(local_index, field_name);
                    if let Some(v) = Value::from_literal_expr(value, self.type_table) {
                        self.interpreter.bind_field(local_index, field_name, v);
                    }
                }
                _ => {
                    // `(*p).field = …` / `q.outer.inner = …` —
                    // unknown receiver, drop every aliased local's
                    // fields.
                    self.interpreter.invalidate_aliased_fields();
                }
            },
            _ => {
                // Index / Deref / something else: opaque write.
                self.interpreter.invalidate_aliased_fields();
            }
        }
        changed |= self.interpreter.reduce_local(expr);
        changed
    }

    /// After a non-Assign expression's children have been walked,
    /// update the field env to reflect side-effects that may have
    /// mutated aliased state. Calls drop every aliased local's
    /// fields; `&mut local` escapes a mutable reference and drops
    /// `local`'s entry; struct / tuple / variant constructors that
    /// capture an aliased local invalidate aliased fields too.
    fn update_field_env_from_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Call { args, func, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
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
            NirExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref hides &mut self: the receiver may have
                // been mutated by the call.
                if let NirExprKind::Local { index, .. } = &receiver.kind {
                    self.interpreter.invalidate_local(*index);
                }
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.interpreter.invalidate_local(*index);
                    }
                }
                self.interpreter.invalidate_aliased_fields();
            }
            NirExprKind::IndirectCall { .. } | NirExprKind::CmRawCall { .. } => {
                // Indirect callee is unknown — closures may capture
                // and mutate any aliased local.
                self.interpreter.invalidate_aliased_fields();
            }
            NirExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let NirExprKind::Local { index, .. } = &inner.kind {
                    self.interpreter.invalidate_local(*index);
                }
            }
            NirExprKind::StructLiteral { fields, .. } => {
                let aliased = self.interpreter.aliased_locals();
                if fields
                    .iter()
                    .any(|f| value_captures_aliased_local(&f.value, aliased))
                {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            NirExprKind::TupleLiteral { elements, .. }
            | NirExprKind::ArrayLiteral { elements, .. } => {
                let aliased = self.interpreter.aliased_locals();
                if elements
                    .iter()
                    .any(|e| value_captures_aliased_local(e, aliased))
                {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            NirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload
                    && value_captures_aliased_local(p, self.interpreter.aliased_locals())
                {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            _ => {}
        }
    }

    /// After a statement is walked, capture any introduced binding into
    /// the interpreter's env so subsequent uses can fold against it.
    fn update_env_from_stmt(&mut self, stmt: &NirStmt) {
        if let NirStmtKind::Let {
            local_index,
            is_mut,
            value,
            ..
        } = &stmt.kind
        {
            let lat = if *is_mut {
                // `let mut x = …` — any later `x = …` would
                // invalidate the binding anyway. The interpreter
                // doesn't track flow-sensitive values for mutable
                // locals, so be conservative up front.
                Lattice::NonConst
            } else {
                self.interpreter.reduce_to_lattice(value)
            };
            // Drop any prior knowledge keyed by this index (rare —
            // a fresh `let` typically introduces a unique index, but
            // defensive). This also clears stale field entries from a
            // same-index reuse before we record new ones below.
            self.interpreter.invalidate_local(*local_index);
            self.interpreter.bind_local(*local_index, lat);
            self.update_field_env_from_let(*local_index, value);
        }
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
    fn update_field_env_from_let(&mut self, local_index: u32, value: &NirExpr) {
        // Unwrap a chained `$value_copy$T<id>(arg)` so the underlying
        // source's knowledge is what we read.
        let inner = match recognize_value_copy(value, self.value_copy_helpers) {
            Some(arg) => arg,
            None => value,
        };
        // Peer through Block / LabeledBlock tails: the producing tail
        // is the final expression of an unlabeled block, or the value
        // of the sole `break label: value` of a labeled block.
        if let Some(tail) = single_producing_tail(inner) {
            self.update_field_env_from_let(local_index, tail);
            return;
        }
        match &inner.kind {
            NirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    if let Some(v) = Value::from_literal_expr(&f.value, self.type_table) {
                        self.interpreter.bind_field(local_index, &f.name, v);
                    }
                }
            }
            NirExprKind::Local { index: src, .. } => {
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
fn has_non_tail_break_to(label: &str, block: &NirBlock) -> bool {
    if !block_has_break_to(label, block) {
        return false;
    }
    let Some(last) = block.stmts.last() else {
        return false;
    };
    let last_is_tail_break_to_self = matches!(
        &last.kind,
        NirStmtKind::Break {
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
    if block.stmts[..block.stmts.len() - 1]
        .iter()
        .any(|s| stmt_has_break_to(label, s))
    {
        return true;
    }
    if let NirStmtKind::Break {
        value: Some(value), ..
    } = &last.kind
    {
        return expr_has_break_to(label, value);
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
fn single_producing_tail(expr: &NirExpr) -> Option<&NirExpr> {
    match &expr.kind {
        NirExprKind::Block(b) => {
            let last = b.stmts.last()?;
            let NirStmtKind::Expr(tail) = &last.kind else {
                return None;
            };
            Some(tail)
        }
        NirExprKind::LabeledBlock { label, block, .. } => {
            let last = block.stmts.last()?;
            let NirStmtKind::Break {
                label: Some(brk_label),
                value: Some(value),
            } = &last.kind
            else {
                return None;
            };
            if brk_label != label {
                return None;
            }
            // The trailing break is the only producer iff no other
            // stmt in the block, and no sub-expression of the tail
            // value, breaks to the same label.
            if block.stmts[..block.stmts.len() - 1]
                .iter()
                .any(|s| stmt_has_break_to(label, s))
            {
                return None;
            }
            if expr_has_break_to(label, value) {
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
fn block_falls_through(block: &NirBlock, type_table: &TypeTable) -> bool {
    let Some(last) = block.stmts.last() else {
        return true;
    };
    stmt_falls_through(last, type_table)
}

fn stmt_falls_through(stmt: &NirStmt, type_table: &TypeTable) -> bool {
    match &stmt.kind {
        NirStmtKind::Return { .. } | NirStmtKind::Break { .. } | NirStmtKind::Continue => false,
        NirStmtKind::Expr(expr) => !is_never_type(expr.type_id, type_table),
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            !is_never_type(value.type_id, type_table)
        }
        NirStmtKind::LabeledBlock { block, .. } => block_falls_through(block, type_table),
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            // Falls through iff some reachable arm falls through.
            // No `else` ⇒ the implicit (empty) else falls through.
            block_falls_through(then_block, type_table)
                || else_block
                    .as_ref()
                    .is_none_or(|eb| block_falls_through(eb, type_table))
        }
        NirStmtKind::Loop { .. } => true,
    }
}

/// True when `type_id` resolves to [`ResolvedType::Never`] — the
/// uninhabited `!` type the elaborator gives expressions that
/// definitely do not produce a value (`panic(…)`, `unreachable()`,
/// `loop { }`, calls whose return type is `!`).
fn is_never_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(type_id), ResolvedType::Never)
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
/// Used by [`LoopWriteCollector`] to keep `has_external_writes`
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
fn value_captures_aliased_local(expr: &NirExpr, aliased: &IndexSet<u32>) -> bool {
    match &expr.kind {
        NirExprKind::Unary { op, expr: inner } => {
            (matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && matches!(inner.kind, NirExprKind::Local { .. }))
                || value_captures_aliased_local(inner, aliased)
        }
        NirExprKind::Local { index, .. } => aliased.contains(index),
        NirExprKind::FieldAccess { expr: inner, .. } | NirExprKind::Cast { expr: inner, .. } => {
            value_captures_aliased_local(inner, aliased)
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
fn collect_loop_write_effects(block: &NirBlock) -> LoopWriteEffects {
    let mut collector = LoopWriteCollector {
        effects: LoopWriteEffects::default(),
    };
    collector.visit_block(block);
    collector.effects
}

/// Walk down a `local.f.g.…` field chain and return the rooted local
/// index, or `None` if the chain is rooted at something other than a
/// `Local` (e.g. `(*p).f`).
fn root_local_of(expr: &NirExpr) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::FieldAccess { expr: inner, .. } => root_local_of(inner),
        _ => None,
    }
}

struct LoopWriteCollector {
    effects: LoopWriteEffects,
}

impl NirRefVisitor for LoopWriteCollector {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Assign { target, .. } => match &target.kind {
                NirExprKind::Local { index, .. } => {
                    self.effects.reassigned_locals.insert(*index);
                }
                NirExprKind::FieldAccess {
                    expr: inner,
                    field_name,
                    ..
                } => match &inner.kind {
                    NirExprKind::Local { index, .. } => {
                        self.effects
                            .written_fields
                            .insert((*index, field_name.clone()));
                    }
                    _ => {
                        // `(*p).f = …` / `q.outer.inner = …` — opaque
                        // receiver; conservatively treat as an
                        // external write so aliased fields drop.
                        self.effects.has_external_writes = true;
                    }
                },
                NirExprKind::Index { .. } => {
                    // `arr[i] = …` mutates an array element, not any
                    // entry in `field_env` (which records `(local,
                    // fieldname) → Value`, not element-level state).
                    // Mirror `visit_assign`'s opaque-write treatment:
                    // mark `has_external_writes` so an aliased-local
                    // invalidation fires only when the receiver could
                    // be reachable through an external alias. If a
                    // future pass starts tracking element values, the
                    // explicit arm here is the place to add a
                    // finer-grained invalidation.
                    self.effects.has_external_writes = true;
                }
                _ => {
                    // Deref / other lvalue shape.
                    self.effects.has_external_writes = true;
                }
            },
            NirExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => match &inner.kind {
                NirExprKind::Local { index, .. } => {
                    self.effects.mut_borrowed.insert(*index);
                }
                NirExprKind::FieldAccess {
                    expr: receiver,
                    field_name,
                    ..
                } => match &receiver.kind {
                    NirExprKind::Local { index, .. } => {
                        // `&mut local.field` — callee can replace
                        // the field or mutate its interior, so the
                        // cached `(local, field)` entry is stale.
                        self.effects
                            .written_fields
                            .insert((*index, field_name.clone()));
                    }
                    _ => {
                        // `&mut local.f.g` or deeper — fall back to
                        // dropping the whole rooted local, since we
                        // do not track fields-of-fields.
                        if let Some(root) = root_local_of(receiver) {
                            self.effects.mut_borrowed.insert(root);
                        } else {
                            self.effects.has_external_writes = true;
                        }
                    }
                },
                _ => {
                    // `&mut (*p).x`, `&mut arr[i]`, … — opaque
                    // receiver; treat as an external write.
                    self.effects.has_external_writes = true;
                }
            },
            NirExprKind::Call { func, args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.effects.mut_borrowed.insert(*index);
                    }
                }
                // Builtin intrinsics (`array_*`, `select`, `likely` /
                // `unlikely`, `memory_*`, `store_*`, `load_*`,
                // `copy_value`, the i64-128 helpers, …) never mutate
                // user-level struct fields tracked by `field_env`:
                // `array_set` writes an array element, `memory_grow`
                // resizes linear memory, and so on. Skipping
                // `has_external_writes` for them lets a `.used` /
                // `.repr` binding made before the loop survive past
                // the loop boundary, even when the body reads /
                // writes the underlying buffer.
                if !is_field_env_pure_call(func) {
                    self.effects.has_external_writes = true;
                }
            }
            NirExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref hides `&mut self`: receiver may be mutated.
                if let NirExprKind::Local { index, .. } = &receiver.kind {
                    self.effects.mut_borrowed.insert(*index);
                }
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.effects.mut_borrowed.insert(*index);
                    }
                }
                self.effects.has_external_writes = true;
            }
            NirExprKind::IndirectCall { .. } | NirExprKind::CmRawCall { .. } => {
                self.effects.has_external_writes = true;
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
