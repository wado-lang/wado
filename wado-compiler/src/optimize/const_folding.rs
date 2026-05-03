//! Constant folding optimization for Wado TIR.
//!
//! Walks every function body and applies the [`tiri::Interpreter`]
//! rewrite rules at each visited node. All reduction logic
//! (literal folding, integer cast collapsing, short-circuit identity
//! rules, env-aware local lookup) lives in [`crate::tiri`]; this module
//! is only the visitor glue that drives `reduce_local` across function
//! bodies and feeds the interpreter's local-variable env from `Let`
//! statements and assignments.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{FunctionRef, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeTable};
use crate::tir_visitor::{
    TirOptVisitor, TirRefVisitor, opt_walk_block, opt_walk_expr, opt_walk_stmt,
};
use crate::tiri::{CalleeMap, GlobalEnv, Interpreter, Lattice, is_ctfe_eligible};

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
    // Build the GlobalEnv once per pass — every immutable global whose
    // initializer reduces to a constant becomes a `GlobalVarGet`
    // rewrite target. Subsumes the legacy `const_propagation` pass,
    // which only recognized literal initializers.
    let globals = build_global_env(project, &type_table, &callees);
    let mut visitor = ConstFoldVisitor {
        interpreter: Interpreter::new(&type_table),
    };
    visitor.interpreter.with_callees(&callees);
    visitor.interpreter.with_globals(&globals);
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            // Local indices are unique per function, not project-wide,
            // so reset the interpreter's env at every function boundary.
            visitor.interpreter.enter_function();
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
}

impl TirOptVisitor for ConstFoldVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        // Loops have a back-edge: a value assigned at the bottom of the
        // body re-enters the top on the next iteration. The single-pass
        // visitor walks each body just once, so an env entry that
        // *predates* the loop would otherwise feed the body's first
        // walk an iteration-1 value that no longer holds at the
        // back-edge — the second iteration's `cond`, the third's, and
        // so on. Pre-invalidate every local whose value the loop body
        // can change so the body walks against the meet of all
        // iteration entries (i.e. `NonConst` for any mutated cell).
        if let TirStmtKind::Loop { body } = &stmt.kind {
            self.invalidate_locals_assigned_in(body);
        }

        // Bottom-up: walk children first so the RHS of `let x = …` is
        // already folded by the time we record `x` in env.
        let changed = opt_walk_stmt(self, stmt);
        self.update_env_from_stmt(stmt);
        changed
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: walk every child kind first (`opt_walk_expr` covers
        // If / Block / Match / Call / … in addition to the Binary /
        // Unary / Cast trees that the interpreter recurses into).
        // Then ask the interpreter to apply local rewrites at this node.
        let mut changed = opt_walk_expr(self, expr);
        // Observe assignments to invalidate the LHS local in env. Done
        // *after* walking so the RHS sees the prior binding.
        if let TirExprKind::Assign { target, .. } = &expr.kind
            && let TirExprKind::Local { index, .. } = &target.kind
        {
            self.interpreter.invalidate_local(*index);
        }
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
                self.interpreter.bind_local(*local_index, lat);
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
