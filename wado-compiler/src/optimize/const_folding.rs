//! Constant folding optimization for Wado TIR.
//!
//! Walks every function body and applies the [`tiri::Interpreter`]
//! rewrite rules at each visited node. All reduction logic
//! (literal folding, integer cast collapsing, short-circuit identity
//! rules, env-aware local lookup, **field-aware local-field reads**)
//! lives in [`crate::tiri`]; this module is only the visitor glue
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
//! See `optimize::field_forward::build_alias_info` /
//! `build_value_copy_helpers` for the per-function alias / helper
//! computations the visitor consumes.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::name::ModuleSource;
use crate::tir::{
    FunctionRef, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::tir_visitor::{
    TirOptVisitor, TirRefVisitor, opt_walk_block, opt_walk_expr, opt_walk_stmt,
};
use crate::tiri::{CalleeMap, GlobalEnv, Interpreter, Lattice, Value, is_ctfe_eligible};

use super::alias::{build_alias_info, build_value_copy_helpers, recognize_value_copy};

/// Apply constant folding to all functions in the project.
pub fn fold_constants(project: &mut FlatPackage) -> bool {
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
    // knowledge across deep copies.
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
                build_alias_info(body, &address_taken, &stores_aliased, &type_table);
            visitor.interpreter.set_alias_info(alias_info);
            changed |= visitor.visit_block(body);
        }
    }
    changed
}

/// Pre-build the [`CalleeMap`] from every CTFE-eligible function in
/// `project`. The map stores `Rc<RefCell<TirFunction>>` handles
/// aliased with `project.functions`, so rebuilding the map every
/// optimizer iteration costs only refcount bumps. The key shape
/// `(module_source, full_name)` mirrors what `try_call_fold`
/// synthesises from a `Call` node's `FunctionRef`.
fn build_callee_map(project: &FlatPackage) -> CalleeMap {
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
    project: &FlatPackage,
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
    /// (the same trick `field_forward::update_knowledge_from_let`
    /// does).
    value_copy_helpers: &'a IndexMap<(ModuleSource, String), TypeId>,
}

impl TirOptVisitor for ConstFoldVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        // Control-flow stmts need branch-aware field-env handling so
        // a `local.field = …` inside a branch doesn't leak as known
        // field knowledge to code that runs only when the branch was
        // skipped. Locals don't need this fork (the only mutation
        // channel is `let mut`, recorded preemptively as `NonConst`),
        // so the existing single-walk env handling stays intact.
        match &mut stmt.kind {
            TirStmtKind::Loop { body } => {
                // Loop back-edge: any local assigned in the body must
                // be `NonConst` for the body's first walk.
                self.invalidate_locals_assigned_in(body);
                // Loops can re-execute and re-assign anything; drop
                // outer field knowledge entirely. (Mirrors
                // field_forward's `forward_in_stmt` Loop arm.)
                self.interpreter.clear_fields();
                let changed = self.visit_block(body);
                self.interpreter.clear_fields();
                return changed;
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                // Sequential scope: outer knowledge flows in, but a
                // `break label: value` inside could skip writes that
                // would otherwise have invalidated entries — drop on
                // exit.
                let changed = self.visit_block(block);
                self.interpreter.clear_fields();
                return changed;
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut changed = self.visit_expr(condition);
                let snap = self.interpreter.snapshot_fields();
                changed |= self.visit_block(then_block);
                self.interpreter.restore_fields(snap);
                if let Some(eb) = else_block {
                    changed |= self.visit_block(eb);
                }
                self.interpreter.clear_fields();
                return changed;
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                let mut changed = self.visit_expr(scrutinee);
                let snap = self.interpreter.snapshot_fields();
                changed |= self.visit_block(then_block);
                self.interpreter.restore_fields(snap);
                if let Some(eb) = else_block {
                    changed |= self.visit_block(eb);
                }
                self.interpreter.clear_fields();
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

    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // `Assign { target, value }` is special-cased: the OUTER `target`
        // expression is an lvalue (write position) and tiri's leaf
        // rewrites — particularly the `FieldAccess(Local, field)`
        // arm — would happily fold a known field-value into the LHS,
        // turning `obj.f = newval` into `5 = newval`. Only `target`'s
        // sub-expressions (the receiver of a `FieldAccess`, the
        // indexee of an `Index`) are read positions; walk those, but
        // leave the outer `target` shape opaque. After the walk,
        // observe what was assigned so the field env stays in sync.
        if matches!(expr.kind, TirExprKind::Assign { .. }) {
            return self.visit_assign(expr);
        }

        // Branch / scope expressions — fork or clear field state to
        // mirror field_forward's `forward_in_expr` semantics.
        match &mut expr.kind {
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut changed = self.visit_expr(condition);
                let snap = self.interpreter.snapshot_fields();
                changed |= self.visit_block(then_branch);
                self.interpreter.restore_fields(snap);
                if let Some(eb) = else_branch {
                    changed |= self.visit_block(eb);
                }
                self.interpreter.clear_fields();
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let mut changed = self.visit_expr(scrutinee);
                for arm in arms {
                    let snap = self.interpreter.snapshot_fields();
                    if let Some(g) = &mut arm.guard {
                        changed |= self.visit_expr(g);
                    }
                    changed |= self.visit_expr(&mut arm.body);
                    self.interpreter.restore_fields(snap);
                }
                self.interpreter.clear_fields();
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                let mut changed = self.visit_expr(scrutinee);
                for arm in arms.iter_mut() {
                    let snap = self.interpreter.snapshot_fields();
                    changed |= self.visit_block(arm);
                    self.interpreter.restore_fields(snap);
                }
                let snap = self.interpreter.snapshot_fields();
                changed |= self.visit_block(default);
                self.interpreter.restore_fields(snap);
                self.interpreter.clear_fields();
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            TirExprKind::Block(b) => {
                // Sequential scope; outer knowledge flows in. After
                // the block, an interior `break label: value` could
                // have skipped some writes, so clear conservatively.
                let mut changed = self.visit_block(b);
                self.interpreter.clear_fields();
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            TirExprKind::LabeledBlock { block, .. } => {
                let mut changed = self.visit_block(block);
                self.interpreter.clear_fields();
                changed |= self.interpreter.reduce_local(expr);
                return changed;
            }
            TirExprKind::Closure { body, .. } => {
                // Closure body executes in its own scope; clear
                // before walking so the body sees a clean slate, and
                // clear again after so outer code doesn't pick up
                // anything leaked.
                self.interpreter.clear_fields();
                let mut changed = self.visit_expr(body);
                self.interpreter.clear_fields();
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

    fn visit_block(&mut self, block: &mut TirBlock) -> bool {
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
    fn visit_assign(&mut self, expr: &mut TirExpr) -> bool {
        let TirExprKind::Assign { target, value } = &mut expr.kind else {
            unreachable!("visit_assign called on non-Assign");
        };
        let mut changed = self.visit_expr(value);
        match &mut target.kind {
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => {
                changed |= self.visit_expr(inner);
            }
            _ => {}
        }
        // Field-env update based on the (post-walk) shape.
        match &target.kind {
            TirExprKind::Local { index, .. } => {
                // Invalidates the local's lattice AND drops any field
                // knowledge tied to it. Captures field-knowledge
                // transfer for `dst = src` (Local→Local copy on a ref
                // type, where both names alias the same heap object).
                self.interpreter.invalidate_local(*index);
                let dst = *index;
                self.update_field_env_from_let(dst, value);
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => match &inner.kind {
                TirExprKind::Local { index, .. } => {
                    let local_index = *index;
                    let field_name = field_name.clone();
                    self.interpreter
                        .invalidate_field(local_index, &field_name);
                    if let Some(v) = Value::from_literal_expr(value, self.type_table) {
                        self.interpreter.bind_field(local_index, &field_name, v);
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
    fn update_field_env_from_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call { args, func, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let TirExprKind::Local { index, .. } = &arg.expr.kind
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
            TirExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref hides &mut self: the receiver may have
                // been mutated by the call.
                if let TirExprKind::Local { index, .. } = &receiver.kind {
                    self.interpreter.invalidate_local(*index);
                }
                for arg in args {
                    if arg.is_mut
                        && let TirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.interpreter.invalidate_local(*index);
                    }
                }
                self.interpreter.invalidate_aliased_fields();
            }
            TirExprKind::IndirectCall { .. } | TirExprKind::CmRawCall { .. } => {
                // Indirect callee is unknown — closures may capture
                // and mutate any aliased local.
                self.interpreter.invalidate_aliased_fields();
            }
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let TirExprKind::Local { index, .. } = &inner.kind {
                    self.interpreter.invalidate_local(*index);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                if fields
                    .iter()
                    .any(|f| value_captures_aliased_local(&f.value, &self.aliased_set()))
                {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            TirExprKind::TupleLiteral { elements, .. } => {
                if elements
                    .iter()
                    .any(|e| value_captures_aliased_local(e, &self.aliased_set()))
                {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload
                    && value_captures_aliased_local(p, &self.aliased_set())
                {
                    self.interpreter.invalidate_aliased_fields();
                }
            }
            _ => {}
        }
    }

    /// After a statement is walked, capture any introduced binding into
    /// the interpreter's env so subsequent uses can fold against it.
    fn update_env_from_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                is_mut,
                value,
                ..
            } => {
                let lat = if *is_mut {
                    // `let mut x = …` — any later `x = …` would
                    // invalidate the binding anyway. The interpreter
                    // doesn't track flow-sensitive values for mutable
                    // locals, so be conservative up front.
                    Lattice::NonConst
                } else {
                    self.interpreter.reduce_to_lattice(value)
                };
                // Drop any prior knowledge keyed by this index (rare
                // — a fresh `let` typically introduces a unique
                // index, but defensive). This also clears stale field
                // entries from a same-index reuse before we record
                // new ones below.
                self.interpreter.invalidate_local(*local_index);
                self.interpreter.bind_local(*local_index, lat);
                self.update_field_env_from_let(*local_index, value);
            }
            // LetDestructure binds multiple locals via pattern matching
            // (`let [a, b] = tuple`). Tuple-aware lattice values aren't
            // modelled yet, so leave the destructured locals
            // Unevaluated. They'll resolve to NonConst the first time
            // they're observed in env, which is the correct
            // conservative answer.
            TirStmtKind::LetDestructure { .. } => {}
            _ => {}
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
    /// - `Call($value_copy$T(src))`: same as above; `dst` is a fresh
    ///   deep copy carrying the same field values.
    fn update_field_env_from_let(&mut self, local_index: u32, value: &TirExpr) {
        // Unwrap a chained `$value_copy$T<id>(arg)` so the underlying
        // source's knowledge is what we read.
        let inner = match recognize_value_copy(value, self.value_copy_helpers) {
            Some(arg) => arg,
            None => value,
        };
        match &inner.kind {
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    if let Some(v) = Value::from_literal_expr(&f.value, self.type_table) {
                        self.interpreter.bind_field(local_index, &f.name, v);
                    }
                }
            }
            TirExprKind::Local { index: src, .. } => {
                self.interpreter.copy_fields_from(*src, local_index);
            }
            _ => {}
        }
    }

    /// Borrow the interpreter's `aliased` set for predicate use in
    /// `value_captures_aliased_local`. A small bridge that hides the
    /// `alias_info` indirection from call sites.
    fn aliased_set(&self) -> &IndexSet<u32> {
        self.interpreter.aliased_locals()
    }

    /// Walk `block` (and every nested expression / statement) collecting
    /// every `Local` index that appears as the target of an `Assign`,
    /// then invalidate each in env. Conservative — any mutation inside
    /// the loop body is treated as making the local non-constant for
    /// the entire loop, which is the only sound choice without modelling
    /// loop iteration.
    fn invalidate_locals_assigned_in(&mut self, block: &TirBlock) {
        let mut collector = AssignedLocalsCollector {
            targets: IndexSet::default(),
        };
        collector.visit_block(block);
        for idx in collector.targets {
            self.interpreter.invalidate_local(idx);
        }
    }
}

/// True when an expression appearing as a struct / tuple / variant
/// field value would hand the freshly-built aggregate access to an
/// already-aliased local. Mirrors `field_forward::value_captures_alias`
/// — we keep a local copy here rather than re-export so a future
/// removal of the field_forward module doesn't break the const-fold
/// path.
fn value_captures_aliased_local(expr: &TirExpr, aliased: &IndexSet<u32>) -> bool {
    match &expr.kind {
        TirExprKind::Unary { op, expr: inner } => {
            (matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                && matches!(inner.kind, TirExprKind::Local { .. }))
                || value_captures_aliased_local(inner, aliased)
        }
        TirExprKind::Local { index, .. } => aliased.contains(index),
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            value_captures_aliased_local(inner, aliased)
        }
        _ => false,
    }
}

/// Read-only walk that records every `Local` index assigned to inside
/// the visited subtree. Drives the loop back-edge invalidation in
/// [`ConstFoldVisitor::invalidate_locals_assigned_in`].
struct AssignedLocalsCollector {
    targets: IndexSet<u32>,
}

impl TirRefVisitor for AssignedLocalsCollector {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Assign { target, .. } = &expr.kind
            && let TirExprKind::Local { index, .. } = &target.kind
        {
            self.targets.insert(*index);
        }
        self.walk_expr(expr);
    }
}
