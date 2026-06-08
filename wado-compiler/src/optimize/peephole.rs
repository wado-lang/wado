//! Unified peephole engine pass.
//!
//! Runs the position-flexible local peephole rewrite rules — short `push_str`
//! simplification (`string_push`), array-literal materialization
//! (`array_literal`), write-only local elimination (`elide_local`), the
//! environment-free subset of constant folding (`const_folding::ConstFoldRule`
//! — literal arithmetic and pure CTFE), and trivial-block / dead-statement
//! pruning (`const_branch_prune::BranchPruneRule`) — over one shared worklist
//! per function: a single engine session (parent map, use index, post-order
//! seed) on which all the rules interleave.
//! See `docs/wep-2026-06-05-nir-rewrite-engine-design.md`.
//!
//! Constant folding is only partly here. Its flow-sensitive folds — env-bound
//! locals, forwarded struct fields, immutable-global reads, and constant-branch
//! collapse — need the driving visitor's per-function dataflow state and stay
//! with the standalone `const_folding::fold_constants` walker. The engine rule
//! handles only the folds that depend on a node and its already-folded children
//! plus the program-wide CTFE callee map, applied through the engine's edit API
//! so the worklist and use index stay coherent.
//!
//! Two rules keep their own standalone engine passes:
//!
//! - `match_to_switch` must run before every other in-loop pass (so they see
//!   the `Switch` shape) and also runs at `-O0`, where the loop is skipped.
//! - `select_lowering` is a terminal post-loop lowering (`If` → `select`) that
//!   must run after all other transformations.
//!
//! The pass is invoked at two points in the fixed-point loop — before `inline`
//! (after value-copy elision / demotion so `string_push` sees the stripped
//! receiver) and after `inline` (so `array_literal` sees the exposed
//! `array_new + push` window). `array_literal` no-ops in the first run and
//! `string_push` no-ops in the second; both bail immediately on a non-matching
//! node, so the wasted dispatch is negligible.

use cranelift_entity::EntityRef;

use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;

use super::array_literal::{Collapser, resolve_array_push_names};
use super::gate::{FunctionGate, GatedPass};
use super::const_branch_prune::{BranchPruneRule, PruneMode};
use super::const_folding::{ConstFoldRule, build_callee_map};
use super::elide_local::ElideRule;
use super::string_push::{ShortPushStrRule, resolve_ctx};

/// Run the unified peephole rule set over every function body. Returns whether
/// any rule fired. Gated: skips functions unchanged since this pass last ran.
pub(super) fn run_peephole(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    // Whole-package contexts, resolved once before the mutable body walk.
    let push_names = resolve_array_push_names(project);
    let array_rule = Collapser::new(&push_names);
    let push_rule = resolve_ctx(project).map(ShortPushStrRule::new);
    // Environment-free constant folding shares the session. It needs the
    // program-wide CTFE callee map and the type table; the per-function `env`
    // stays empty so only literal arithmetic and pure CTFE fold here, leaving
    // the flow-sensitive folds to the standalone `const_folding` walker.
    let type_table = project.type_table.borrow();
    let callees = build_callee_map(project);
    let const_fold_rule = ConstFoldRule::new(&type_table, &callees);
    let branch_prune_rule = BranchPruneRule::new(PruneMode::Fixpoint);

    let len = project.functions.len();
    gate.run_gated(GatedPass::Peephole, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        // `stores_aliased_locals` is per-function, so the elide rule is rebuilt
        // for each body.
        let stores_aliased = func.stores_aliased_locals.clone();
        let elide_rule = ElideRule::new(&stores_aliased);
        let Some(body) = func.body.as_mut() else {
            return false;
        };
        let mut rules: Vec<&dyn Rule> = vec![
            &array_rule,
            &elide_rule,
            &const_fold_rule,
            &branch_prune_rule,
        ];
        if let Some(push_rule) = push_rule.as_ref() {
            rules.push(push_rule);
        }
        let mut engine = Engine::new(body);
        engine.run(&rules)
    })
}
