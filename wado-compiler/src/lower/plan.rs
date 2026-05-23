//! Pre-fold planning. Mutations are confined to the type table,
//! additive synthesis (`flat.{structs,functions,globals}` growth),
//! and per-function declaration-shape edits in
//! [`boxing::shadow_params`] / [`lift_mut`]. Every
//! expression-shape rewrite lives in
//! [`crate::lower::translate`].
//!
//! Sub-pass ordering is dictated by what the next pass reads:
//! `boxing::prepare_types` mutates the type table before any
//! analysis that consults resolved types; `lift_mut` runs before
//! `value_copy::plan`'s seed walker so the lifted `Let mut`
//! statements are visible.
//!
//! See `docs/wep-2026-05-11-nir.md`.

use crate::flat_package::FlatPackage;

pub mod boxing;
pub mod closure;
pub mod globals;
pub mod lift_mut;
pub mod string;
pub mod value_copy;

pub struct LowerPlan {
    pub box_plan: boxing::BoxPlan,
    pub closure: closure::ClosurePlan,
    pub value_copy: value_copy::ValueCopyPlan,
}

pub fn plan(flat: &mut FlatPackage) -> LowerPlan {
    globals::extract(flat);
    let box_plan = boxing::prepare_types(flat);
    boxing::shadow_params(flat, &box_plan);
    let closure = closure::plan(flat);
    globals::build_initialize_modules(flat);
    flat.rebuild_variant_indices();
    lift_mut::lift_mut_match_bindings(flat);
    let value_copy = value_copy::plan(flat);
    LowerPlan {
        box_plan,
        closure,
        value_copy,
    }
}
