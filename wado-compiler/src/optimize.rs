//! Optimization passes for Wado NIR, rewriting the [`NirPackage`] in place. Each
//! pass documents itself in its own module under `optimize/`; the sequence of
//! `run_pass` calls below is the only statement of what runs in what order.
//! `docs/optimizer.md` is the reader-facing inventory, and WEP 2026-06-05 covers
//! the two-tier NIR, the rewrite engine, and the gate.

mod aggregate_forward;
mod alias;
mod arena_query;
mod clone_forward;
mod cold_outline;
mod condition_implication;
mod const_branch_prune;
mod const_folding;
mod const_object_globalization;
mod container_sroa;
mod copy_prop;
mod dae;
pub mod dce;
mod drop_value;
mod drve;
mod elide_box_local;
mod elide_local;
mod extract;
mod field_scalarize;
mod gate;
mod if_chain_to_match;
mod inline;
mod labeled_block_fusion;
mod let_block_flatten;
mod licm;
mod loop_version_bce;
mod match_to_switch;
mod mod_ref;
pub(crate) mod multi_value_return;
mod param_spec;
mod peephole;
mod ref_elim;
mod scalar_forward;
mod select_lowering;
mod sroa;
mod sroa_param;
mod sroa_variant_return;
mod store_load_forward;
mod string_push;
mod tmpl_hoist;
mod tuple_projection;
mod value_copy;
mod value_copy_demote;

use const_branch_prune::{prune_constant_branches, prune_template_block_wrappers};
use const_folding::{ConstFoldCache, fold_constants, fold_constants_all};
use const_object_globalization::globalize_const_objects;
use container_sroa::scalarize_containers;
use copy_prop::propagate_copies;
use dae::eliminate_dead_arguments;
use dce::{
    analyze_dce, filter_bytes_literals, filter_string_literals,
    remove_unreachable_closure_functors, remove_unreachable_functions, remove_unreachable_globals,
    remove_unreachable_types, unhoist_unobserved_globals,
};
use drve::eliminate_dead_return_values;
use field_scalarize::scalarize_hot_fields;
use inline::inline_functions;
use licm::apply_licm;
use match_to_switch::{match_to_switch_all, match_to_switch_globals};
use scalar_forward::forward_scalar_temps;
use sroa::scalar_replace_aggregates;
use sroa_param::sroa_single_field_parameters;
use sroa_variant_return::scalarize_variant_returns;
use store_load_forward::forward_stores_to_loads_all;
use tmpl_hoist::hoist_template_buffers;
use value_copy_demote::demote_value_copies;

use extract::FreezePhase;
use gate::GatedPass;

use crate::compiler_host::SpanEmitter;
use crate::nir_package::NirPackage;

/// Configuration for optimization passes
struct OptConfig {
    /// Number of fixed-point iterations
    iterations: u32,
    /// What one copy of a callee may cost to run, for it to be worth splicing.
    inline_threshold: usize,
    /// Percent the inliner may grow the whole unit by, however many copies the
    /// threshold would otherwise admit, or `None` to leave it unbounded. See
    /// [`inline::InlineBudget`].
    inline_growth: Option<u32>,
    /// Whether exhausting `iterations` is a defect rather than a budget — true
    /// for the caps `-O2`/`-Os` and `-O3` size so the loop converges under them.
    cap_is_defect: bool,
}

/// Optimization level. Every level runs DCE and the post-loop rewrites the Wasm
/// backend depends on; `optimize` below sets each one's loop and inline budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimization loop. DCE plus the always-on post-optimization
    /// rewrites required by the Wasm backend.
    O0,
    /// Development optimizations. All passes with fast iteration count.
    O1,
    /// Production optimizations. All passes including DCE.
    #[default]
    O2,
    /// Aggressive production optimizations. All passes including DCE.
    O3,
    /// Size optimizations. Same as O2 plus name section stripping.
    /// Intended for frontend/browser deployment.
    Os,
}

/// Longest string literal materialized as a constant `array.new_fixed<u8>`
/// rather than a passive data segment; above it the compact repr is kept and the
/// global stays lazy. `-O3` trades code size for more eager string globals,
/// while `-Os` and the rest stay conservative. A global hoisted by
/// `const_object_globalization` overrides it via `prefer_fixed_string_repr`.
fn string_inline_max_bytes(opt_level: OptLevel) -> usize {
    match opt_level {
        OptLevel::O3 => 8,
        _ => NirPackage::DEFAULT_STRING_INLINE_MAX_BYTES,
    }
}

/// The optimizer's entry point. Every level runs DCE, which cuts codegen work
/// sharply; `O1` and above run it twice, before the fixed-point loop to shrink
/// the working set and after it to sweep what the loop made dead. What runs in
/// between is the pass sequence below, scaled by [`OptLevel`] and by whatever
/// `opt` overrides of that level's defaults.
pub fn optimize(
    mut project: NirPackage,
    opt_level: OptLevel,
    opt: crate::OptOverrides,
    profiler: &dyn SpanEmitter,
) -> NirPackage {
    let crate::OptOverrides {
        inline_threshold,
        inline_growth,
        iterations: opt_iterations,
    } = opt;
    // Decide the short-string inline threshold once, from the opt level. Read
    // by `wir_build` (`translate_packed_array` / `register_literal_data`) to
    // pick a constant `array.new_fixed<u8>` repr for strings at or below it —
    // which lets a constant string global promote to an eager Wasm constant.
    project.string_inline_max_bytes = string_inline_max_bytes(opt_level);
    match opt_level {
        OptLevel::O0 => {
            // No optimizations, but still run DCE to reduce codegen work
            run_dce(&mut project, profiler);
            // Dense-int / dense-enum `Match` → `Switch` is a codegen-
            // friendly late lowering. The translator emits a canonical
            // `Match` (see WEP 2026-05-11). Materialising `Switch` here
            // — even at -O0 — keeps wir_build's `br_table` path live
            // for dense matches when the optimizer loop is skipped.
            // The synthesised default-arm call resolves to
            // `builtin::unreachable`, which lowers to Wasm
            // `unreachable` directly and is never DCE'd, so ordering
            // around DCE is irrelevant.
            run_pass("nir/match_to_switch", &mut project, profiler, |p| {
                // O0 skips the loop/gate entirely; lower every function.
                match_to_switch_all(p)
            });
        }
        OptLevel::O1 => {
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(2),
                inline_threshold: inline_threshold.unwrap_or(4),
                inline_growth,
                cap_is_defect: false,
            };
            // Early DCE: remove unreachable functions/types before optimization
            // to reduce the working set for subsequent passes
            run_dce(&mut project, profiler);
            run_optimization_passes(&mut project, &config, profiler);
            // Final DCE: clean up code made dead by optimizations
            run_dce(&mut project, profiler);
        }
        OptLevel::O2 | OptLevel::Os => {
            let config = OptConfig {
                // Every source in the tree settles by 8, so 15 is slack.
                iterations: opt_iterations.unwrap_or(15),
                // Threshold 16, best of three alternating whole-suite runs
                // against 13: the cbor serialize rows gain 11-13% and eight
                // more 3-7%, against json-catalog ser at -3%. That row turns
                // first as the threshold climbs — -8.2% at 20, -35.3% at 26 —
                // so 16 costs it least while still buying the serde rows.
                // Below 13 every row is worse.
                inline_threshold: inline_threshold.unwrap_or(16),
                inline_growth,
                cap_is_defect: opt_iterations.is_none(),
            };
            run_dce(&mut project, profiler);
            run_optimization_passes(&mut project, &config, profiler);
            run_dce(&mut project, profiler);
            if opt_level == OptLevel::Os {
                project.strip_names = true;
            }
        }
        OptLevel::O3 => {
            // The heaviest source in the tree — the Gale-generated SQLite parser
            // in `benchmark/sqlite_parse` — converges in 6, so 20 is slack.
            //
            // Threshold 26 is the top of what shakes NIR into new shapes for
            // its cost: on the Gale CSS3 parser 28, 30 and 32 all emit ~959KB
            // in ~13.5s off the same candidate set, while 26 emits 719KB in
            // 10.7s. The corpus barely reads the knob (24 → 32 moves its
            // compile CPU 4.6%), so the cost is felt compiling one heavy file.
            let config = OptConfig {
                iterations: opt_iterations.unwrap_or(20),
                inline_threshold: inline_threshold.unwrap_or(26),
                inline_growth,
                cap_is_defect: opt_iterations.is_none(),
            };
            run_dce(&mut project, profiler);
            run_optimization_passes(&mut project, &config, profiler);
            run_dce(&mut project, profiler);
        }
    }

    // FieldAccess promotion: scalar fields over stable receivers freeze to
    // operands (born-as-operands), so store-load forwarding / cse become
    // operand reads. Runs after the optimization loop (so the struct shape is
    // post-SROA) but BEFORE select_lowering / multi_value_return, which rewrite
    // the body into WIR-shaped forms the value-graph build would misread. The
    // two soundness gates (scalar-field + receiver-availability) and the WIR
    // const-local propagation keep it default-safe.
    if opt_level != OptLevel::O0 {
        run_pass("nir/promote_fields", &mut project, profiler, |p| {
            extract::freeze_pure_arith(p, /* include_fields */ true, FreezePhase::Terminal)
        });
        // Re-run the structural BCE matcher now that `promote_fields` froze
        // invariant bounds (`arr.used`) into constant operands the in-loop
        // cond-impl could not see; pair with `const_branch_prune` so the
        // now-`false` checks' panic blocks are removed. Must precede
        // `select_lowering`, which reshapes conditions out of matcher form.
        run_bounded_fixpoint("nir/cond_impl_post_promote", &mut project, profiler, |p| {
            condition_implication::eliminate_post_promote(p) | prune_constant_branches(p)
        });
        // Loop-versioned BCE: a check whose bound is loop-invariant but not
        // statically related to the loop guard (the relation lives at the
        // call site) is deleted in a fast clone guarded by the runtime
        // residual `H < B`; the original loop is kept as the slow arm. Runs
        // after `cond_impl_post_promote` so only statically-unprovable
        // checks are versioned, and before `select_lowering`, which
        // reshapes conditions out of matcher form.
        run_pass("nir/loop_version_bce", &mut project, profiler, |p| {
            loop_version_bce::version_loops(p)
        });
    }

    // Post-optimization rewrites: select lowering for branchless Wasm
    run_pass("nir/select_lowering", &mut project, profiler, |p| {
        select_lowering::select_lowering(p)
    });

    // Multi-value return ABI classification: marks an aggregate-returning
    // function whose every return builds a fresh literal and whose every call
    // site destructures the bound temp, which WIR build reads to emit the
    // multi-value signature and split the call-site binding. Runs after every
    // other transformation, so the analysis sees the final NIR shape.
    run_pass("nir/multi_value_return", &mut project, profiler, |p| {
        multi_value_return::classify_multi_value_returns(p)
    });

    // Freeze re-emittable pure arithmetic (constants / local reads composed by
    // Binary/Unary/Cast) into operand values, materialised by the WIR
    // extractor. Runs last so no binary-walking pass sees the promoted form;
    // orphaned arith / local-read nodes become unreachable from the skeleton
    // root and are simply not emitted. (Early arith promotion already ran before
    // the loop; `FieldAccess` promotion ran above, after SROA.)
    run_pass("nir/freeze_pure_arith", &mut project, profiler, |p| {
        extract::freeze_pure_arith(p, /* include_fields */ false, FreezePhase::Terminal)
    });

    // The born-resolved invariant is now enforced by the type system: a call
    // node's `func_id` is a non-optional `FuncId`, stamped at its synthesis site.

    project
}

fn run_dce(project: &mut NirPackage, profiler: &dyn SpanEmitter) {
    profiler.span_start("nir/dce");
    // Compute every reachability set up front, then apply mutations
    // in order. `remove_unreachable_globals` rewrites function bodies
    // (it drops `GlobalVarSet` for dead globals), which can orphan
    // calls inside dropped initializers; the *final* `run_dce` after
    // the optimization loop cleans those up — the savings of an extra
    // mid-loop call-graph rebuild aren't worth its cost.
    // Before the census: a global whose value nothing observes still counts as
    // read — by its own guard, and by the bindings folding left behind — so it
    // has to lose those first to fall out below.
    unhoist_unobserved_globals(project);
    let analysis = analyze_dce(project);
    remove_unreachable_functions(project, &analysis.functions);
    remove_unreachable_globals(project, &analysis.globals);
    filter_string_literals(project);
    remove_unreachable_types(project, &analysis);
    filter_bytes_literals(project);
    remove_unreachable_closure_functors(project);
    project.rebuild_variant_indices();
    profiler.span_end("nir/dce");
}

/// Defensive iteration cap for the post-loop cleanup fixpoints
/// (`cond_impl_post_promote`, `branch_prune_final`, `const_fold_post_global`).
/// In practice these converge in a handful of rounds; the cap exists only so an
/// oscillating rewrite pair cannot hang compilation, mirroring the bounded
/// iteration count of the main fixed-point loop.
const POST_LOOP_FIXPOINT_CAP: u32 = 100;

/// Run `step` to a fixed point under [`run_pass`] instrumentation, bounded by
/// [`POST_LOOP_FIXPOINT_CAP`]. Returns whether any round changed the IR; emits a
/// debug diagnostic if the cap is reached (a sign of an oscillating rewrite).
fn run_bounded_fixpoint(
    name: &'static str,
    project: &mut NirPackage,
    profiler: &dyn SpanEmitter,
    mut step: impl FnMut(&mut NirPackage) -> bool,
) -> bool {
    run_pass(name, project, profiler, |p| {
        let mut changed = false;
        for i in 0..POST_LOOP_FIXPOINT_CAP {
            if !step(p) {
                break;
            }
            changed = true;
            if i + 1 == POST_LOOP_FIXPOINT_CAP {
                profiler.debug(&format!(
                    "{name} hit the post-loop fixpoint cap ({POST_LOOP_FIXPOINT_CAP}); \
                     stopping (possible oscillating rewrite)"
                ));
            }
        }
        changed
    })
}

/// Run a single optimization pass with profiling, returning whether it changed anything.
///
/// Honours the `WADO_LIST_PASSES`, `WADO_DUMP_PASS_BEFORE`, and
/// `WADO_DUMP_PASS_AFTER` developer-debug env vars (see `pass_dump`).
fn run_pass(
    name: &str,
    project: &mut NirPackage,
    profiler: &dyn SpanEmitter,
    f: impl FnOnce(&mut NirPackage) -> bool,
) -> bool {
    pass_dump::list_pass(name);
    if pass_dump::should_skip_pass(name) {
        return false;
    }
    pass_dump::dump_nir(name, project, pass_dump::Phase::Before);
    profiler.span_start(name);
    let changed = f(project);
    profiler.span_end(name);
    pass_dump::dump_nir(name, project, pass_dump::Phase::After);
    changed
}

pub mod pass_dump {
    use std::sync::{Mutex, OnceLock};

    use super::NirPackage;
    use crate::hashmap::IndexMap;
    use crate::wir::WirPackage;

    #[derive(Copy, Clone)]
    pub enum Phase {
        Before,
        After,
    }

    impl Phase {
        fn env_var(self) -> &'static str {
            match self {
                Self::Before => "WADO_DUMP_PASS_BEFORE",
                Self::After => "WADO_DUMP_PASS_AFTER",
            }
        }

        fn label(self) -> &'static str {
            match self {
                Self::Before => "before",
                Self::After => "after",
            }
        }
    }

    fn dump_before_list() -> &'static Vec<String> {
        static LIST: OnceLock<Vec<String>> = OnceLock::new();
        LIST.get_or_init(|| {
            crate::trace::parse_env_list(std::env::var(Phase::Before.env_var()).ok().as_deref())
        })
    }

    fn dump_after_list() -> &'static Vec<String> {
        static LIST: OnceLock<Vec<String>> = OnceLock::new();
        LIST.get_or_init(|| {
            crate::trace::parse_env_list(std::env::var(Phase::After.env_var()).ok().as_deref())
        })
    }

    fn list_passes_enabled() -> bool {
        static FLAG: OnceLock<bool> = OnceLock::new();
        *FLAG.get_or_init(|| std::env::var("WADO_LIST_PASSES").is_ok())
    }

    fn skip_list() -> &'static Vec<String> {
        static LIST: OnceLock<Vec<String>> = OnceLock::new();
        LIST.get_or_init(|| {
            crate::trace::parse_env_list(std::env::var("WADO_SKIP_PASS").ok().as_deref())
        })
    }

    /// Returns true if `name` matches one of the comma-separated entries in
    /// the `WADO_SKIP_PASS` env var. Each entry is matched against the bare
    /// pass name (e.g., `nir/ref_elim`) and against `<pass>@<n>` where `<n>`
    /// is the 1-based occurrence number — letting bisection target a
    /// specific iteration (e.g., `nir/ref_elim@2`).
    pub fn should_skip_pass(name: &str) -> bool {
        static COUNTS: OnceLock<Mutex<IndexMap<String, u32>>> = OnceLock::new();
        let list = skip_list();
        if list.is_empty() {
            return false;
        }
        let mut counts = COUNTS
            .get_or_init(|| Mutex::new(IndexMap::default()))
            .lock()
            .unwrap();
        let n = counts.entry(name.to_string()).or_insert(0);
        *n += 1;
        let scoped = format!("{name}@{n}");
        list.iter().any(|s| s == name || s == &scoped)
    }

    fn matches(name: &str, phase: Phase) -> bool {
        let list = match phase {
            Phase::Before => dump_before_list(),
            Phase::After => dump_after_list(),
        };
        list.iter().any(|n| n == name)
    }

    pub fn list_pass(name: &str) {
        if list_passes_enabled() {
            eprintln!("[pass] {name}");
        }
    }

    pub fn dump_nir(name: &str, project: &NirPackage, phase: Phase) {
        if matches(name, phase) {
            let label = phase.label();
            eprintln!("=== NIR {label} {name} ===");
            eprintln!("{}", crate::nir_unparse::unparse_nir_package(project));
            eprintln!("=== end NIR {label} {name} ===");
        }
    }

    pub fn dump_wir(name: &str, module: &WirPackage, phase: Phase) {
        if matches(name, phase) {
            let label = phase.label();
            eprintln!("=== WIR {label} {name} ===");
            eprintln!("{}", crate::wir_unparse::unparse_wir(module));
            eprintln!("=== end WIR {label} {name} ===");
        }
    }
}

/// Run the NIR passes to a fixed point, exiting early once an iteration changes
/// nothing. Each pass's position is justified in the comment beside its call.
fn run_optimization_passes(
    project: &mut NirPackage,
    config: &OptConfig,
    profiler: &dyn SpanEmitter,
) {
    let mut descriptor_cache = dce::DescriptorCache::default();
    // Before anything prices a body, so `nir/inline`'s cold discount describes
    // the function it copies. Ahead of the gate too, so the call graph is built
    // over the split shape rather than growing into it.
    run_pass("nir/cold_outline", project, profiler, |p| {
        cold_outline::outline_cold_regions(p, &mut descriptor_cache)
    });
    // Per-function dirty-set gate. Every loop pass is gate-aware:
    // a per-function pass (`gated!`) skips functions unchanged since it last ran;
    // an interprocedural pass scans all functions but reports exactly the ones
    // it touched. Both go through `&mut gate`.
    let mut gate = gate::FunctionGate::new(project);
    // Cross-iteration cache for `const_fold`'s whole-program maps (CalleeMap /
    // GlobalEnv / GlobalFieldEnv). Rebuilt only when the function or global count
    // changes; see `ConstFoldCache`.
    let mut const_fold_cache: Option<ConstFoldCache> = None;
    let mut param_spec_state = param_spec::ParamSpecState::default();
    // Held across the loop: the budget anchors on the unit as the loop found it,
    // so what the rounds add together stays bounded. See `InlineBudget`.
    let mut inline_budget = inline::InlineBudget::new(config.inline_growth);
    // Dense `Match` → `Switch` in global initializer bodies. Functions are
    // lowered by `MatchToSwitchRule` inside the unified peephole session; the
    // function-level loop never mutates global initializer bodies, so a single
    // pass over globals here is equivalent to running it each iteration.
    run_pass("nir/match_to_switch_globals", project, profiler, |p| {
        match_to_switch_globals(p)
    });
    // Operand-promotion keystone. Pure values are
    // frozen into `Operand::Value` before the value passes, so the passes read
    // operands (`engine.operand_value`) instead of rebuilding the value graph.
    // Arith only here (before the loop); `FieldAccess` promotion runs late (after
    // the SROA passes), since SROA scalarizes the structs a promoted `FieldAccess`
    // would reference. See the late call in `optimize`.
    run_pass("nir/promote_pure_values_early", project, profiler, |p| {
        extract::freeze_pure_arith(p, /* include_fields */ false, FreezePhase::Early)
    });
    // What changed in the iteration just run, and so the convergence flag:
    // empty ends the loop, non-empty after it names what held the loop open.
    let mut iter_changed: Vec<&'static str> = Vec::new();
    for i in 0..config.iterations {
        profiler.span_start(&format!("nir/iteration {}", i + 1));
        iter_changed.clear();
        macro_rules! record {
            ($name:expr, $c:expr) => {{
                if $c {
                    iter_changed.push($name);
                }
            }};
        }
        // A gate-aware pass: receives `&mut gate`, skips functions it has
        // already processed at their current revision, and reports its own
        // per-function changes.
        //
        // A drained column skips the round at the schedule, not inside the
        // pass. A pass builds its whole-program state before it reaches its
        // first function, and a drained column means no function changed since
        // it last ran, so that round would rebuild the state and rewrite
        // nothing.
        macro_rules! gated {
            (@run $name:expr, $id:expr, $pass:expr) => {
                run_pass($name, project, profiler, |p| {
                    gate.any_pending($id, p.functions.len()) && $pass(p, &mut gate)
                })
            };
            ($name:expr, $id:expr, $pass:expr) => {{
                record!($name, gated!(@run $name, $id, $pass));
            }};
        }
        // Reports changes to the gate but not to `iter_changed` — must never
        // keep the loop alive on its own.
        macro_rules! gate_only {
            ($name:expr, $id:expr, $pass:expr) => {{
                gated!(@run $name, $id, $pass);
            }};
        }
        // Container SROA must run before inline in each iteration: inline
        // expands `IndexValue::index_value` and friends into raw
        // `builtin::array_get_value` + field-access pairs, after which the method-call
        // shape this pass keys on is gone. Running early also catches the `[]`
        // desugaring while its inner `Constructor` is still a plain `Call`.
        gated!(
            "nir/container_sroa",
            GatedPass::ContainerSroa,
            scalarize_containers
        );
        // Peephole engine, pre-inline run — several rules over one worklist; see
        // `optimize/peephole.rs`. Before inline so `string_push` still sees the
        // `buf.push_str("0.")` shape, which the inliner's expansion would erase.
        // Hosts `MatchToSwitchRule` (`include_match = true`), so `inline` copies
        // `Switch`-shaped bodies, and `const_branch_prune`.
        gated!("nir/peephole", GatedPass::PeepholePre, |p, g| {
            peephole::run_peephole(p, g, true)
        });
        // Demote deep `$value_copy$T` copies of `List<E>` to shallow spine
        // copies when the binding's elements are provably never mutated through
        // it. Runs before `nir/inline`, where the `$value_copy$T(arg)` shape it
        // matches disappears. (Copies are inserted precisely at the lower phase,
        // so there is no elision pass to sequence against.)
        gate_only!(
            "nir/value_copy_demote",
            GatedPass::ValueCopyDemote,
            |p, g| demote_value_copies(p, g, &mut descriptor_cache)
        );
        // Single-field parameter SROA: rewrite functions whose parameter type
        // is `&S` for a single-field struct (`Box<T>` being the canonical
        // case) to take the inner scalar directly. Runs before `nir/inline`
        // so the inliner sees post-SROA signatures and can propagate the
        // scalar through call chains. NIR analog of WIR's `sroa_param`; see
        // `optimize/sroa_param.rs`.
        gated!(
            "nir/sroa_param",
            GatedPass::SroaParam,
            sroa_single_field_parameters
        );
        // Variant returns become tuple returns: `Result<T, E>` -> `[tag, slots]`.
        // Runs beside `sroa_param` and before `nir/inline` for the same reason —
        // the inliner, and every value pass after it, then sees an integer tag
        // and plain locals where a boxed variant used to be opaque. The Wasm ABI
        // is left to `multi_value_return`, which already flattens a destructured
        // tuple return.
        gated!(
            "nir/sroa_variant_return",
            GatedPass::SroaVariantReturn,
            scalarize_variant_returns
        );
        // `inline` self-reports the callers it modified to the gate (no
        // `bump_all`); it only mutates caller bodies, so the gated passes need
        // re-examine just those (and their neighbours).
        gated!("nir/inline", GatedPass::Inline, |p, g| inline_functions(
            p,
            config.inline_threshold,
            &mut inline_budget,
            g,
            &mut descriptor_cache,
        ));
        // Peephole engine, post-inline run. `elide_local` runs again over
        // inline's freshly dead bindings. No `MatchToSwitchRule` — the
        // pre-inline run lowered every reachable `Match` already.
        gated!("nir/peephole", GatedPass::PeepholePost, |p, g| {
            peephole::run_peephole(p, g, false)
        });
        // `labeled_block_fusion` moved into the post-inline `nir/peephole`
        // session as `LabeledBlockFusionRule`; see `optimize/peephole.rs`.
        gated!(
            "nir/let_block_flatten",
            GatedPass::LetBlockFlatten,
            let_block_flatten::flatten_let_blocks
        );
        gated!("nir/sroa", GatedPass::Sroa, scalar_replace_aggregates);
        gated!("nir/copy_prop", GatedPass::CopyProp, propagate_copies);
        // DAE / DRVE after `copy_prop` shrinks signatures and discards unused
        // let-bindings before `const_fold` revisits the simplified body.
        // Running here (rather than at WIR level) lets `inline` see the slimmer
        // signatures on the next iteration and lets `dce` clean up the freshly
        // dead computation in the same fixed-point loop. (Write-only local
        // elimination moved into the unified `nir/peephole` pass above.)
        gated!("nir/dae", GatedPass::Dae, eliminate_dead_arguments);
        gated!("nir/drve", GatedPass::Drve, eliminate_dead_return_values);
        // The flow-sensitive half of constant folding — env-bound locals,
        // forwarded struct fields, immutable-global reads, constant-branch
        // collapse — where the env-free half runs in `nir/peephole`. It absorbs
        // `field_forward`, which used to alternate one statement per round and
        // left `-O3` non-convergent. No `cse` pass: hash-consing already shares.
        gated!("nir/const_fold", GatedPass::ConstFold, |p, g| {
            fold_constants(p, g, &mut const_fold_cache)
        });
        // Trivial-block / dead-statement pruning moved into the pre-inline
        // `nir/peephole` run above; the post-loop `branch_prune_final` and the
        // post-globalization `const_fold_post_global` keep their own engine
        // sessions (`prune_template_block_wrappers` / `prune_constant_branches`).
        // After `const_fold`, so a caller's config struct literal already holds
        // folded constants; inside the loop, so the next iteration prunes the
        // clone's dead branches and reaches one call deeper. Not `gated!`: it
        // owns no column, since a summary taken before a callee gained a write
        // would license an unsound substitution (see its module doc).
        record!(
            "nir/param_spec",
            run_pass("nir/param_spec", project, profiler, |p| {
                param_spec::specialize_const_params(p, &mut param_spec_state, &mut gate)
            })
        );
        gated!("nir/licm", GatedPass::Licm, apply_licm);
        gated!(
            "nir/tmpl_hoist",
            GatedPass::TmplHoist,
            hoist_template_buffers
        );
        profiler.span_end(&format!("nir/iteration {}", i + 1));
        crate::compiler_trace!(
            "opt_loop",
            "iter {:>3}: changed_by = [{}]",
            i + 1,
            iter_changed.join(", ")
        );
        if iter_changed.is_empty() {
            profiler.debug(&format!(
                "NIR optimizer converged after {} iteration(s)",
                i + 1
            ));
            break;
        }
    }
    if let Some(report) = inline_budget.report() {
        profiler.debug(&report);
    }
    if !iter_changed.is_empty() {
        let report = format!(
            "NIR optimizer hit the {}-iteration cap without converging; still changing: [{}]",
            config.iterations,
            iter_changed.join(", ")
        );
        profiler.debug(&report);
        // A pass reported a change it did not make, or is taking one step per
        // round where one walk reaches its fixpoint. A release build pays it.
        debug_assert!(!config.cap_is_defect, "{report}");
    }
    // Hot Field Scalarization runs once after the main loop converges.
    // Running inside the loop would cause the write-back/re-read stmts it
    // inserts to be counted as new field accesses on the next iteration,
    // triggering spurious re-scalarization of the same fields.
    run_pass(
        "nir/field_scalarize",
        project,
        profiler,
        scalarize_hot_fields,
    );
    // Forward the scalarization shadow inits (`__hfs_x = obj.f`) to constants.
    // `field_scalarize` runs after the fixed-point loop, so no in-loop
    // `store_load_forward` sees its shadow reads; this once-over folds an
    // `obj.f` whose field the ValueGraph still knows (`obj` built from a
    // constant literal upstream) into the literal, before globalization. Runs
    // before `const_object_globalization` so it never sees the nullable
    // `GlobalVarGet`s globalization emits.
    run_pass(
        "nir/store_load_forward_post_scalarize",
        project,
        profiler,
        forward_stores_to_loads_all,
    );
    // Final cleanup: flatten any `__tmpl:` labeled blocks the fixpoint
    // preserved as anchors for `tmpl_hoist`. `tmpl_hoist` has finished
    // by now (it runs inside the fixpoint loop), so the wrappers are
    // pure overhead — peel them so codegen emits the inner straight-line
    // body directly. Iterate until convergence because one flatten can
    // expose another (e.g. single-stmt Block collapse on a freshly
    // produced `Block { Expr(tail) }`).
    run_bounded_fixpoint("nir/branch_prune_final", project, profiler, |p| {
        prune_template_block_wrappers(p)
    });
    // Body globalization: hoist constant, read-only aggregate `let` bindings
    // into shared immutable module globals so they build once at instantiation
    // (WEP-2026-05-31). Runs once after the fixpoint converges, on the stable
    // post-optimization shape, so the read-only gate and the const-aggregate
    // recognizer see fully-inlined / array-literal-materialized bindings. The
    // inline `GlobalVarSet`s it emits are promoted to eager Wasm constants by
    // `wir_optimize::const_global`; the final `run_dce` reclaims the dead
    // binding locals.
    run_pass("nir/const_object_globalization", project, profiler, |p| {
        globalize_const_objects(p)
    });
    // Clean up after globalization: fold the `global:X.used` length reads it
    // exposes (recovered via `const_folding`'s `GlobalFieldEnv`) and prune the
    // now-constant bounds-check branches, so a hoisted constant-index array
    // keeps the bounds-check elimination it had as a local. Only `const_fold` /
    // `branch_prune` run here — re-entering the full loop is unsafe, since the
    // nullable `GlobalVarGet`s globalization emits are not meant to flow back
    // through `value_copy` / `sroa` (which is why globalization runs last).
    run_bounded_fixpoint("nir/const_fold_post_global", project, profiler, |p| {
        fold_constants_all(p) | prune_constant_branches(p)
    });
    // Forward the inliner's leftover single-use pure-scalar value-parameter
    // temps into their uses. Runs last, after every scalarization / globalization
    // recognizer has matched its shape, so it only strips dead-weight locals.
    run_pass("nir/scalar_forward", project, profiler, |p| {
        forward_scalar_temps(p, &mut gate)
    });
    // Forward read-only clones that inlining + const-object globalization leave
    // behind (`array_clone(&array_clone(&__const_obj))` into a read-only
    // binding). Runs last: `const_object_globalization` — which creates the
    // const global the residual clone reads — is itself a post-loop pass, so the
    // flat `let t = array_clone(&global.field)` shape only exists here, after the
    // pre-inline value-copy elider that cannot reach it has long run.
    run_pass("nir/clone_forward", project, profiler, |p| {
        clone_forward::forward_redundant_clones(p)
    });
}
