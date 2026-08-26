//! Pre-fold planning (WEP 2026-05-11). Mutations stay confined to the type
//! table, additive synthesis, and the declaration-shape edits in
//! [`boxing::shadow_params`] / [`lift_mut`]; every expression-shape rewrite
//! lives in [`crate::lower::translate`]. Sub-pass order follows what the next
//! pass reads: the type table is mutated before any analysis consults it.

use crate::flat_package::FlatPackage;
use crate::logger::{Bail, ErrorSink};

pub mod boxing;
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

/// Set each parameter's `is_mut_ref` from its type while it is still a
/// `MutRef`. `boxing::prepare_types` then collapses `&mut T` and `&T` onto the
/// same `Box<T>`, so this is the last point where the two are distinguishable.
/// A `&T` cannot be written through, so only a `&mut` parameter can mutate the
/// caller's argument storage — the fact the value-copy elision oracle needs.
fn capture_param_mut_ref(flat: &mut FlatPackage) {
    let type_table = flat.type_table.borrow();
    for func in &flat.functions {
        let mut func = func.borrow_mut();
        for param in &mut func.params {
            param.is_mut_ref = matches!(
                type_table.get(param.type_id),
                crate::tir::ResolvedType::MutRef(_)
            );
        }
    }
}

/// Functions whose first parameter is a reference (`&self` / `&mut self`),
/// captured before `boxing::prepare_types` erases the reference into `Box<T>`.
fn ref_receiver_methods(flat: &FlatPackage) -> value_copy::funcset::FuncKeySet {
    let type_table = flat.type_table.borrow();
    let mut set = value_copy::funcset::FuncKeySet::default();
    for f in &flat.functions {
        let f = f.borrow();
        let Some(p0) = f.params.first() else { continue };
        if matches!(
            type_table.get(p0.type_id),
            crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
        ) {
            set.insert(f.module_source.clone(), f.name.clone());
        }
    }
    set
}

pub fn plan(flat: &mut FlatPackage, errors: &dyn ErrorSink) -> Result<LowerPlan, Bail> {
    globals::extract(flat, errors)?;
    // Record each parameter's `&mut`-ness before `boxing::prepare_types`
    // rewrites `&mut T` / `&T` to the same `Box<T>`, erasing the distinction.
    capture_param_mut_ref(flat);
    // The pre-boxing analyses share one graph: what follows rewrites bodies but
    // adds no function, so the callee sets do not move until closure lifting.
    let pre_boxing_calls = value_copy::callgraph::CallGraph::build(flat);
    // Before boxing, while `&mut T` still names its referent: a detached `&mut`
    // argument becomes a borrow of a temp that is stored back after the call.
    mut_ref_writeback::insert_write_backs(flat, &pre_boxing_calls, errors)?;
    // Confinement and receiver-ref capture run before boxing collapses `&mut T`
    // / `&T` onto `Box<T>`; both results key on parameter position, unchanged by
    // boxing.
    let confined_params = value_copy::confine::compute_confined_params(flat, &pre_boxing_calls);
    let ref_receiver_methods = ref_receiver_methods(flat);
    let box_plan = boxing::prepare_types(flat);
    boxing::shadow_params(flat, &box_plan);
    let lifted_from = flat.functions.len();
    let closure = closure::plan(flat);
    boxing::shadow_new_functions(flat, &box_plan, lifted_from);
    globals::build_initialize_modules(flat);
    flat.rebuild_variant_indices();
    lift_mut::lift_mut_match_bindings(flat);
    let value_copy = value_copy::plan(flat, confined_params, ref_receiver_methods);
    Ok(LowerPlan {
        box_plan,
        closure,
        value_copy,
    })
}
