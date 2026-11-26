//! Unified peephole engine pass (WEP 2026-06-05): every position-flexible local
//! rewrite rule interleaved over one engine session per function rather than a
//! session apiece, invoked on each side of `inline` per fixed-point iteration.
//! [`PeepholeRule`] declares each rule's order and the sides it runs on.

use std::sync::Arc;

use cranelift_entity::EntityRef;

use crate::hashmap::IndexSet;
use crate::nir::{FuncId, FuncParts};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::niri::{build_callee_map, build_ctfe_builtin_map};

use super::aggregate_forward::AggregateForwardRule;
use super::arena_query::MarkerBuiltins;
use super::closure_devirt::build_closure_devirt;
use super::const_branch_prune::BranchPruneRule;
use super::const_folding::ConstFoldRule;
use super::drop_value::DropValueRule;
use super::elide_box_local::build_elide_box_local;
use super::elide_local::ElideRule;
use super::gate::{FunctionGate, GatedPass};
use super::identity_cast::{FieldThroughRefRule, IdentityCastRule, IdentityDerefRule};
use super::if_chain_to_match::IfChainToMatchRule;
use super::known_case::KnownCaseRule;
use super::labeled_block_fusion::{LabeledBlockFusionRule, SlotTempSroaRule};
use super::match_to_bitset::MatchToBitsetRule;
use super::match_to_switch::MatchToSwitchRule;
use super::ref_elim::build_ref_elim;
use super::string_push::{AppendFuseRule, ConstAsciiPushRule, resolve_ctx};
use super::tuple_projection::TupleProjectionRule;
use crate::optimize::heap_effect::{HeapEffectsCache, LazyHeapFrame};
use crate::optimize::match_to_switch::intern_cold_markers;
use crate::optimize::mod_ref::SummaryCache;
use crate::optimize::pass_dump;
use crate::optimize::select_lowering::intern_select;

/// Which side of `nir/inline` a peephole run sits on. The two apply different
/// rule sets, so each keeps a gate column of its own: a function quiescent for
/// one must still be revisited by the other.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum InlineSide {
    Pre,
    Post,
}

impl InlineSide {
    fn gated_pass(self) -> GatedPass {
        match self {
            Self::Pre => GatedPass::PeepholePre,
            Self::Post => GatedPass::PeepholePost,
        }
    }
}

/// Run the peephole rules of `side` over every function body. Returns whether
/// any rule fired. Gated: skips functions unchanged since this pass last ran.
pub(super) fn run_peephole(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    side: InlineSide,
    heap: &mut HeapEffectsCache,
    mod_ref: &mut SummaryCache,
) -> bool {
    // Intern the builtins the bundled rules synthesize, before any shared
    // immutable borrow of `project`, so their calls are born resolved.
    let (cold_path_id, unreachable_id) = intern_cold_markers(project);
    let select_id = intern_select(project);
    // Whole-package contexts, resolved once before the mutable body walk.
    let push_ctx = resolve_ctx(project);
    let const_ascii_push_rule = push_ctx.as_ref().and_then(ConstAsciiPushRule::new);
    let append_fuse_rule = push_ctx.as_ref().and_then(AppendFuseRule::new);
    let len = project.functions.len();
    let gated_pass = side.gated_pass();
    let type_table = project.type_table.borrow();
    let pending: IndexSet<FuncId> = gate.pending(gated_pass, len).into_iter().collect();
    let callees = build_callee_map(project, gate.exec(), |id| pending.contains(&id));
    let ctfe_builtins = build_ctfe_builtin_map(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let markers = MarkerBuiltins::of(project);
    let branch_prune_rule = BranchPruneRule { markers };
    let bitset_rule = MatchToBitsetRule::new(select_id);
    let match_rule = MatchToSwitchRule::new(cold_path_id, unreachable_id);
    let known_case_rule = KnownCaseRule { markers };
    let closure_devirt_rule = build_closure_devirt(project);
    let heap_effects =
        (side == InlineSide::Post).then(|| heap.effects(project, &type_table, gate.exec()));

    debug_assert!(
        gate.any_pending(gated_pass, len),
        "[NIR] run_peephole entered on a drained gate: the setup above would be \
         built for a run that visits no function. `gated!` owns that skip."
    );
    // Decided here: the rules run on worker threads, which a skip scope does
    // not reach.
    let enabled: Vec<PeepholeRule> = PeepholeRule::ALL
        .iter()
        .copied()
        .filter(|rule| {
            rule.runs_on(side) && pass_dump::decides(&format!("nir/peephole/{}", rule.name()))
        })
        .collect();
    // Once for the run.
    let summaries = mod_ref.summaries(project, gate.exec());
    let type_table = &*type_table;
    let functions = &project.functions;
    let exec = Arc::clone(gate.exec());
    let changed = gate.run_gated_par(gated_pass, len, |buffers, fid| {
        let mut func = functions[fid.index()].borrow_mut();
        let elide_rule = ElideRule::new(summaries.of_body(fid.index()));
        // Per function, as its CTFE budget and remembered misses are: what one
        // body spends must not decide whether the next one folds.
        let const_fold_rule = ConstFoldRule::new(type_table, &callees, &ctfe_builtins);
        let Some(pristine) = func.body.as_ref() else {
            return false;
        };
        // Both analyse the body before any rule of the session edits it.
        let ref_elim_rule = heap_effects.as_ref().map(|effects| {
            let params = func.params.iter().map(|p| p.local_index).collect();
            build_ref_elim(pristine, LazyHeapFrame::new(effects, params), type_table)
        });
        let elide_box_rule = (side == InlineSide::Post)
            .then(|| build_elide_box_local(pristine, &func.address_taken_locals));
        // Disjoint borrow of the body arena and the local list so rules can
        // both rewrite the body and allocate fresh locals via the engine.
        let FuncParts {
            body,
            locals,
            params,
            ..
        } = func.parts();
        let body = body.expect("checked above");
        let mut rules: Vec<&dyn Rule> = Vec::with_capacity(enabled.len());
        for &rule in &enabled {
            let instance: &dyn Rule = match rule {
                PeepholeRule::IfChainToMatch => &IfChainToMatchRule,
                PeepholeRule::MatchToBitset => &bitset_rule,
                PeepholeRule::MatchToSwitch => &match_rule,
                PeepholeRule::RefElim => ref_elim_rule.as_ref().expect("built post-inline"),
                PeepholeRule::ElideBoxLocal => elide_box_rule.as_ref().expect("built post-inline"),
                PeepholeRule::LabeledBlockFusion => &LabeledBlockFusionRule,
                PeepholeRule::SlotTempSroa => &SlotTempSroaRule,
                PeepholeRule::DropValue => &DropValueRule,
                PeepholeRule::ClosureDevirt => &closure_devirt_rule,
                PeepholeRule::KnownCase => &known_case_rule,
                PeepholeRule::IdentityCast => &IdentityCastRule,
                PeepholeRule::IdentityDeref => &IdentityDerefRule,
                PeepholeRule::FieldThroughRef => &FieldThroughRefRule,
                PeepholeRule::AggregateForward => &AggregateForwardRule,
                PeepholeRule::Elide => &elide_rule,
                PeepholeRule::ConstFold => &const_fold_rule,
                PeepholeRule::BranchPrune => &branch_prune_rule,
                PeepholeRule::TupleProjection => &TupleProjectionRule,
                // Absent where the package holds none of the `String` methods
                // they retarget to.
                PeepholeRule::ConstAsciiPush => match &const_ascii_push_rule {
                    Some(r) => r,
                    None => continue,
                },
                PeepholeRule::AppendFuse => match &append_fuse_rule {
                    Some(r) => r,
                    None => continue,
                },
            };
            rules.push(instance);
        }
        let mut engine = Engine::new(body, buffers, locals, params, type_table);
        engine.set_pure_builtin_callees(&pure_builtin_callees);
        let changed = engine.run(&rules);
        let copies = engine.copied_locals().to_vec();
        drop(engine);
        // A copy of a boxed local is boxed too. Dropping the annotation lets a
        // read of it forward past a write through the alias.
        for [of, copy] in copies {
            if func.address_taken_locals.contains(&of) {
                func.address_taken_locals.insert(copy);
            }
        }
        changed
    });
    exec.drop_all(callees);
    changed
}

/// The runs a rule takes part in.
#[derive(Clone, Copy)]
enum Sides {
    Pre,
    Post,
    Both,
}

macro_rules! peephole_rules {
    ($($(#[$doc:meta])* $variant:ident = $name:literal on $sides:ident,)*) => {
        /// A rule of the peephole session, named `nir/peephole/<name>` where a
        /// skip list names it. Declared in the order a session applies them.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum PeepholeRule {
            $($(#[$doc])* $variant,)*
        }

        impl PeepholeRule {
            const ALL: &[Self] = &[$(Self::$variant),*];

            fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            fn runs_on(self, side: InlineSide) -> bool {
                let sides = match self {
                    $(Self::$variant => Sides::$sides,)*
                };
                matches!(
                    (sides, side),
                    (Sides::Both, _) | (Sides::Pre, InlineSide::Pre) | (Sides::Post, InlineSide::Post)
                )
            }
        }
    };
}

peephole_rules! {
    /// Ahead of `MatchToSwitch`, which lowers the `Match` it plants.
    IfChainToMatch = "if_chain_to_match" on Pre,
    /// Ahead of `MatchToSwitch`, taking the boolean sets it would otherwise
    /// make a `br_table` of.
    MatchToBitset = "match_to_bitset" on Pre,
    /// Before `inline`, so it copies `Switch`-shaped bodies. A `Match` a later
    /// rewrite plants is caught by the next round's pre-inline run.
    MatchToSwitch = "match_to_switch" on Pre,
    /// Drops the ref bindings inlining exposes.
    RefElim = "ref_elim" on Post,
    /// Collapses the cells `sroa_param` and `inline` expose.
    ElideBoxLocal = "elide_box_local" on Post,
    /// The `Option` / `Result` an inlined helper builds for its if-let or `?`
    /// consumer.
    LabeledBlockFusion = "labeled_block_fusion" on Post,
    /// Scalarizes the `[tag, slots…]` temp of a `let x = f()?` fusion cannot
    /// relocate. After fusion, which produces the better code where it applies
    /// and whose shape this would otherwise consume.
    SlotTempSroa = "slot_temp_sroa" on Post,
    /// A value-producing labeled block `inline` leaves where a caller discarded
    /// the result. After fusion, whose shapes it would otherwise flatten, and
    /// before `Elide`, which reclaims what it strips.
    DropValue = "drop_value" on Post,
    /// The closure-bearing field read it resolves through is what `inline`
    /// exposes when an iterator adaptor's `next` is copied in. The pre-inline
    /// run takes the reads the passes after `inline` resolved, so that round's
    /// `inline` splices the `$call`.
    ClosureDevirt = "closure_devirt" on Both,
    /// Ahead of the folding rules: the arms it drops are what leaves a constant
    /// condition and a dead branch for them to reclaim.
    KnownCase = "known_case" on Both,
    /// Ahead of every rule matching on the shape of a value: a cast that
    /// changes no representation is what hides that shape.
    IdentityCast = "identity_cast" on Both,
    IdentityDeref = "identity_deref" on Both,
    FieldThroughRef = "field_through_ref" on Both,
    AggregateForward = "aggregate_forward" on Both,
    Elide = "elide" on Both,
    /// The environment-free half: literal arithmetic and pure CTFE. The
    /// flow-sensitive folds are `nir/const_fold`'s.
    ConstFold = "const_fold" on Both,
    BranchPrune = "branch_prune" on Both,
    TupleProjection = "tuple_projection" on Both,
    /// Matches a `push` call, which only the pre-inline run still sees whole.
    ConstAsciiPush = "const_ascii_push" on Both,
    /// After `ConstAsciiPush`: the run it fuses holds the `push_ascii_byte`
    /// calls that one retargets.
    AppendFuse = "append_fuse" on Both,
}
