//! Unified peephole engine pass (WEP 2026-06-05): every position-flexible local
//! rewrite rule interleaved over one engine session per function rather than a
//! session apiece, invoked twice per fixed-point iteration so `string_push` can
//! still see a `push_str` before `inline`.
//! Only the env-free half of constant folding runs here.

use cranelift_entity::EntityRef;

use crate::nir::NirFunction;
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;

use super::aggregate_forward::AggregateForwardRule;
use super::const_branch_prune::{BranchPruneRule, PruneMode};
use super::const_folding::{ConstFoldRule, build_callee_map, build_ctfe_builtin_map};
use super::drop_value::DropValueRule;
use super::elide_box_local::build_elide_box_local;
use super::elide_local::ElideRule;
use super::gate::{FunctionGate, GatedPass};
use super::if_chain_to_match::IfChainToMatchRule;
use super::labeled_block_fusion::{build_labeled_block_fusion, build_slot_temp_sroa};
use super::match_to_switch::MatchToSwitchRule;
use super::ref_elim::build_ref_elim;
use super::string_push::{AppendFuseRule, ConstAsciiPushRule, ShortPushStrRule, resolve_ctx};
use super::tuple_projection::TupleProjectionRule;

/// Run the unified peephole rule set over every function body. Returns whether
/// any rule fired. Gated: skips functions unchanged since this pass last ran.
///
/// `pre_inline` adds `MatchToSwitchRule` (lower every reachable `Match` to
/// `Switch` before `inline` copies bodies; the post-inline run would only
/// re-scan already-`Switch` bodies). A `Match` a later rewrite plants is caught
/// by the next iteration's pre-inline run.
pub(super) fn run_peephole(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    pre_inline: bool,
) -> bool {
    // Intern the builtins the bundled rules synthesize, before any shared
    // immutable borrow of `project`, so their calls are born resolved.
    let (cold_path_id, unreachable_id) = super::match_to_switch::intern_cold_markers(project);
    // Whole-package contexts, resolved once before the mutable body walk.
    let push_ctx = resolve_ctx(project);
    let const_ascii_push_rule = push_ctx.as_ref().and_then(ConstAsciiPushRule::new);
    // Ordered after the two rules above within one session: the run it fuses is
    // what their per-byte expansion produces.
    let append_fuse_rule = push_ctx.as_ref().and_then(AppendFuseRule::new);
    let push_rule = push_ctx.map(ShortPushStrRule::new);
    // Environment-free constant folding shares the session. It needs the
    // program-wide CTFE callee map and the type table; the per-function `env`
    // stays empty so only literal arithmetic and pure CTFE fold here, leaving
    // the flow-sensitive folds to the standalone `const_folding` walker.
    let type_table = project.type_table.borrow();
    let callees = build_callee_map(project);
    let ctfe_builtins = build_ctfe_builtin_map(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let const_fold_rule = ConstFoldRule::new(&type_table, &callees, &ctfe_builtins);
    let branch_prune_rule = BranchPruneRule::new(PruneMode::Fixpoint);
    let aggregate_forward_rule = AggregateForwardRule;
    let match_rule = MatchToSwitchRule::new(&type_table, cold_path_id, unreachable_id);
    let if_chain_rule = IfChainToMatchRule::new(&type_table);
    let tuple_projection_rule = TupleProjectionRule;

    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    // The pre- and post-inline runs apply different rule sets, so they keep
    // separate watermark columns: a function quiescent for one must still be
    // revisited by the other.
    let gated_pass = if pre_inline {
        GatedPass::PeepholePre
    } else {
        GatedPass::PeepholePost
    };
    debug_assert!(
        gate.any_pending(gated_pass, len),
        "[NIR] run_peephole entered on a drained gate: the setup above would be \
         built for a run that visits no function. `gated!` owns that skip."
    );
    // Once for the run.
    let effects = super::mod_ref::compute_fn_effects(&project.functions, &project.builtin_registry);
    gate.run_gated(gated_pass, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        // `stores_aliased_locals` is per-function, so the ref-elimination rule is
        // rebuilt for each body.
        let stores_aliased = func.stores_aliased_locals.clone();
        let elide_rule = ElideRule::new(&stores_aliased, &effects);
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
        // One rule, two entry points: `apply_block` fuses the value-discarding
        // if-let shape, `apply_expr` threads the value-producing `match LB`
        // (`?`) shape. Both only exist post-inline.
        let labeled_block_fusion_rule = (!pre_inline).then(build_labeled_block_fusion);
        // Scalarizes the `[tag, slots…]` temp fusion leaves behind — the
        // value-producing `let x = f()?` consumer it cannot relocate. Ordered
        // after fusion, which produces the better code where it applies and
        // whose shape this rule would otherwise consume.
        let slot_temp_sroa_rule = (!pre_inline).then(|| build_slot_temp_sroa(&type_table));
        // Disjoint borrow of the body arena and the local list so rules can
        // both rewrite the body and allocate fresh locals via the engine.
        let NirFunction { body, locals, .. } = &mut *func;
        let Some(body) = body.as_mut() else {
            return false;
        };
        let mut rules: Vec<&dyn Rule> = Vec::with_capacity(10);
        if pre_inline {
            // Ordered before `match_rule`, which lowers the `Match` it plants.
            rules.push(&if_chain_rule);
            rules.push(&match_rule);
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
        if let Some(slot_temp_sroa_rule) = slot_temp_sroa_rule.as_ref() {
            rules.push(slot_temp_sroa_rule);
        }
        // Post-inline only: a value-producing labeled block in statement position
        // is what `inline` leaves where a caller discarded the result. After
        // fusion, whose shapes it would otherwise flatten, and before
        // `elide_rule`, which reclaims what it strips.
        if !pre_inline {
            rules.push(&DropValueRule);
        }
        rules.extend([
            &aggregate_forward_rule as &dyn Rule,
            &elide_rule,
            &const_fold_rule,
            &branch_prune_rule,
            &tuple_projection_rule,
        ]);
        if let Some(push_rule) = push_rule.as_ref() {
            rules.push(push_rule);
        }
        if let Some(const_ascii_push_rule) = const_ascii_push_rule.as_ref() {
            rules.push(const_ascii_push_rule);
        }
        if let Some(append_fuse_rule) = append_fuse_rule.as_ref() {
            rules.push(append_fuse_rule);
        }
        let mut engine = Engine::new(body, &mut buffers, locals);
        // `MatchToSwitchRule` materializes promoted constant scrutinees / arm
        // bodies; the extractor reads `prim` from the type table for literal repr.
        engine.set_value_graph_type_table(&type_table);
        engine.set_pure_builtin_callees(&pure_builtin_callees);
        engine.run(&rules)
    })
}
