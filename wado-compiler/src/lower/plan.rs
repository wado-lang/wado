//! Pre-fold planning (WEP 2026-05-11). Mutations stay confined to the type
//! table, additive synthesis, and the declaration-shape edits in
//! [`boxing::prepare`] / [`lift_mut`]; every expression-shape rewrite
//! lives in [`crate::lower::translate`]. Sub-pass order follows what the next
//! pass reads: the type table is mutated before any analysis consults it.

use crate::compiler_host::{SpanEmitter, in_span};
use crate::flat_package::FlatPackage;
use crate::logger::{Bail, ErrorSink};

pub mod boxing;
pub mod capture_ref;
pub mod closure;
pub mod globals;
pub mod lift_mut;
pub mod mut_ref_writeback;
pub mod string;
pub mod value_copy;
pub mod whole_value_writes;

pub struct LowerPlan {
    pub box_plan: boxing::BoxPlan,
    pub closure: closure::ClosurePlan,
    pub value_copy: value_copy::ValueCopyPlan,
}

pub fn plan(
    flat: &mut FlatPackage,
    errors: &dyn ErrorSink,
    profiler: &dyn SpanEmitter,
) -> Result<LowerPlan, Bail> {
    in_span(profiler, "lower/globals", || globals::extract(flat, errors))?;
    // The write-back and boxing ask which types are affine.
    value_copy::register_type_templates(flat);
    // Ahead of every analysis, which then reads the proxy as any other reference.
    capture_ref::capture_observed_by_ref(flat);
    // The pre-lifting analyses share one graph: what follows rewrites bodies but
    // adds no function, so the callee sets do not move until closure lifting.
    let pre_lifting_calls = in_span(profiler, "lower/callgraph", || {
        value_copy::callgraph::CallGraph::build(flat)
    });
    // A detached `&mut` argument becomes a reference to a temp that is stored
    // back after the call. The reference a callee keeps is read from its body,
    // so the fixpoint runs here as well as after closure lifting: a write-back
    // needs the answer before the closure plan adds its functions.
    let builtins = flat.builtin_declarations.clone();
    let closure_captures = in_span(profiler, "lower/closure_captures", || {
        value_copy::reach::ClosureCaptures::collect(flat)
    });
    let retained = in_span(profiler, "lower/retention", || {
        value_copy::retention::compute_retention(
            flat,
            &pre_lifting_calls,
            &builtins,
            &closure_captures,
        )
    });
    in_span(profiler, "lower/write_backs", || {
        mut_ref_writeback::insert_write_backs(
            flat,
            &pre_lifting_calls,
            &retained.calls,
            &retained.rows,
            &closure_captures,
            errors,
        )
    })?;
    let confined_params = in_span(profiler, "lower/confine", || {
        value_copy::confine::compute_confined_params(flat, &pre_lifting_calls, &builtins)
    });
    let closure = in_span(profiler, "lower/closure", || closure::plan(flat));
    // After lifting, so a fn-param specialization, a copy of its callee, is
    // moved into cells once.
    let box_plan = in_span(profiler, "lower/boxing", || boxing::prepare(flat));
    globals::build_initialize_modules(flat);
    lift_mut::lift_mut_match_bindings(flat);
    let value_copy = value_copy::plan(
        flat,
        builtins,
        confined_params,
        profiler,
    );
    Ok(LowerPlan {
        box_plan,
        closure,
        value_copy,
    })
}
