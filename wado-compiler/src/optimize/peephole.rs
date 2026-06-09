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
//! `match_to_switch` (dense `Match` → `Switch`) also runs here as a rule:
//! folding it into the shared session means a function's `Match` lowering reuses
//! the same engine the other rules already build, instead of a separate
//! per-function session each iteration. Global initializer bodies are not
//! visited by the function-level loop, so their `Match` lowering runs once via
//! `match_to_switch_globals`, and `-O0` (loop skipped) keeps `match_to_switch_all`.
//!
//! `select_lowering` stays a terminal post-loop lowering (`If` → `select`) that
//! must run after all other transformations.
//!
//! The pass is invoked at two points in the fixed-point loop — before `inline`
//! (where `ValueCopyElideRule` runs in the same session, so `string_push` sees
//! the value-copy-stripped receiver via the shared worklist; `value_copy_demote`
//! then runs after) and after `inline` (so `array_literal` sees the exposed
//! `array_new + push` window, `RefElimRule` cleans up the ref bindings
//! inlining exposes, `ElideBoxLocalRule` collapses the `Box<T>` shells, and
//! `LabeledBlockFusionRule` folds inlined `Option`/`Result` allocations into
//! the consumer's `if-let`/`match` site). `array_literal` no-ops in the first
//! run and `string_push` no-ops in the second; both bail immediately on a
//! non-matching node, so the wasted dispatch is negligible.

use cranelift_entity::EntityRef;

use crate::nir::NirFunction;
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;

use super::array_literal::{Collapser, resolve_array_push_names};
use super::const_branch_prune::{BranchPruneRule, PruneMode};
use super::const_folding::{ConstFoldRule, build_callee_map};
use super::elide_box_local::build_elide_box_local;
use super::elide_local::ElideRule;
use super::gate::{FunctionGate, GatedPass};
use super::labeled_block_fusion::build_labeled_block_fusion;
use super::match_to_switch::MatchToSwitchRule;
use super::ref_elim::build_ref_elim;
use super::string_push::{ShortPushStrRule, resolve_ctx};
use super::value_copy_elide::{ValueCopyElideRule, build_usage};

/// Run the unified peephole rule set over every function body. Returns whether
/// any rule fired. Gated: skips functions unchanged since this pass last ran.
///
/// `pre_inline` adds the rules that the old loop ran once per iteration before
/// `inline`: `MatchToSwitchRule` (lower every reachable `Match` to `Switch`
/// before `inline` copies bodies; the post-inline run would only re-scan
/// already-`Switch` bodies) and `ValueCopyElideRule` (strip read-only
/// `$value_copy$T` wrappers). A `Match` or wrapper a later rewrite plants is
/// caught by the next iteration's pre-inline run, matching the old timing.
pub(super) fn run_peephole(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    pre_inline: bool,
) -> bool {
    // Whole-package contexts, resolved once before the mutable body walk.
    let push_names = resolve_array_push_names(project);
    let array_rule = Collapser::new(&push_names);
    let push_rule = resolve_ctx(project).map(ShortPushStrRule::new);
    // `$value_copy$T` helper types, for the pre-inline value-copy-elision rule.
    let value_copy_set = project.value_copy_helper_types();
    // Environment-free constant folding shares the session. It needs the
    // program-wide CTFE callee map and the type table; the per-function `env`
    // stays empty so only literal arithmetic and pure CTFE fold here, leaving
    // the flow-sensitive folds to the standalone `const_folding` walker.
    let type_table = project.type_table.borrow();
    let callees = build_callee_map(project);
    let const_fold_rule = ConstFoldRule::new(&type_table, &callees);
    let branch_prune_rule = BranchPruneRule::new(PruneMode::Fixpoint);
    let match_rule = MatchToSwitchRule::new(&type_table);

    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::Peephole, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        // `stores_aliased_locals` is per-function, so the elide rule is rebuilt
        // for each body.
        let stores_aliased = func.stores_aliased_locals.clone();
        let elide_rule = ElideRule::new(&stores_aliased);
        // Value-copy elision runs pre-inline only, and not on the
        // `$value_copy$T` helpers themselves. Its usage map is built from the
        // pristine body here, before the session rewrites it (matching the old
        // standalone pass's snapshot); the rule borrows the shared helper-type
        // set.
        let value_copy_usage = (pre_inline && !func.is_value_copy() && !value_copy_set.is_empty())
            .then(|| func.body.as_ref().map(|b| build_usage(b, &type_table)))
            .flatten();
        let value_copy_rule = value_copy_usage.map(|u| ValueCopyElideRule::new(&value_copy_set, u));
        // Reference elimination runs post-inline only (it cleans up the ref
        // bindings inlining exposes). Its maps are built from the pristine
        // post-inline body.
        let ref_elim_rule = (!pre_inline)
            .then(|| func.body.as_ref().map(build_ref_elim))
            .flatten();
        // Adjacent-use box-local elision runs post-inline only (it collapses the
        // `Box<T>` shells `sroa_param` / `inline` expose). Its stats come from
        // the pristine post-inline body; the escape sets (`address_taken` here,
        // `stores_aliased` above) are read off the function before its body is
        // borrowed — `build_elide_box_local` copies them, so they pass by
        // reference (no per-function clone, and none at all in the pre-inline
        // phase where this rule is absent).
        let elide_box_rule = (!pre_inline)
            .then(|| {
                func.body
                    .as_ref()
                    .map(|b| build_elide_box_local(b, &func.address_taken_locals, &stores_aliased))
            })
            .flatten();
        // Labeled-block fusion runs post-inline only: the `let temp = LB { ...;
        // break L: Some(v); }; if VariantTest(temp, …)` shape it folds is what
        // `inline` exposes when an `Option`/`Result`-returning helper is copied
        // into an if-let caller. The rule allocates fresh `__fused_payload_N`
        // locals via the engine, so it sits next to the other block-level
        // rules.
        let labeled_block_fusion_rule = (!pre_inline).then(build_labeled_block_fusion);
        // Disjoint borrow of the body arena and the local list so rules can
        // both rewrite the body and allocate fresh locals via the engine.
        let NirFunction { body, locals, .. } = &mut *func;
        let Some(body) = body.as_mut() else {
            return false;
        };
        let mut rules: Vec<&dyn Rule> = Vec::with_capacity(10);
        if pre_inline {
            rules.push(&match_rule);
        }
        if let Some(value_copy_rule) = value_copy_rule.as_ref() {
            rules.push(value_copy_rule);
        }
        if let Some(ref_elim_rule) = ref_elim_rule.as_ref() {
            rules.push(ref_elim_rule);
        }
        if let Some(elide_box_rule) = elide_box_rule.as_ref() {
            rules.push(elide_box_rule);
        }
        if let Some(labeled_block_fusion_rule) = labeled_block_fusion_rule.as_ref() {
            rules.push(labeled_block_fusion_rule);
        }
        rules.extend([
            &array_rule as &dyn Rule,
            &elide_rule,
            &const_fold_rule,
            &branch_prune_rule,
        ]);
        if let Some(push_rule) = push_rule.as_ref() {
            rules.push(push_rule);
        }
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&rules)
    })
}
