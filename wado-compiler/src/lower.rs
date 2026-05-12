//! Lowering pass for Wado TIR
//!
//! The lower phase is a `FlatPackage` (TIR-shaped) → `NirPackage`
//! translation, structured as planner + translator:
//!
//! ```text
//! FlatPackage → lower::plan::plan(&mut flat) → lower::translate::translate(flat, plan)
//! ```
//!
//! The planner ([`plan::plan`]) runs the TIR-mutating sub-passes
//! (pattern lowering, global-initializer extraction, boxing, closure
//! lowering, value-copy materialization, string-literal collection) and
//! produces a [`plan::LowerPlan`] of facts the translator consumes. The
//! translator ([`translate::translate`]) is a single fold from TIR to
//! NIR; arms that rewrite markers (`builtin::copy_value::<T>` → helper
//! call, `i128`/`u128` match → if-else chain) live there.
//!
//! See `docs/wep-2026-05-11-nir.md`.

pub mod plan;
pub mod translate;
mod wide_int_literal;

use crate::flat_package::FlatPackage;
use crate::nir_package::NirPackage;

/// Lower a [`FlatPackage`] and return a [`NirPackage`].
pub fn lower(mut flat: FlatPackage) -> NirPackage {
    let plan = plan::plan(&mut flat);
    translate::translate(flat, plan)
}
