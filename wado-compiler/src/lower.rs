//! Lowering `FlatPackage` → `NirPackage` (WEP 2026-05-11), as planner then
//! translator. [`plan::plan`] runs the TIR-mutating sub-passes — pattern
//! lowering, global-initializer extraction, boxing, closure lowering, value-copy
//! materialization, string-literal collection — and hands the translator a
//! [`plan::LowerPlan`]. [`translate::translate`] is then a single fold.

pub mod bare_asserts;
pub mod contract_checks;
pub mod plan;
pub mod translate;
pub mod wide_arith;
pub(crate) mod wide_int_literal;

use crate::compiler_host::{SpanEmitter, in_span};
use crate::flat_package::FlatPackage;
use crate::logger::{Bail, ErrorSink};
use crate::nir_package::NirPackage;

/// Lower a [`FlatPackage`] and return a [`NirPackage`].
pub fn lower(
    mut flat: FlatPackage,
    errors: &dyn ErrorSink,
    profiler: &dyn SpanEmitter,
) -> Result<NirPackage, Bail> {
    // Ahead of the plan, which binds the locals a copied clause brings.
    contract_checks::lower(&mut flat);
    let plan = in_span(profiler, "lower/plan", || {
        plan::plan(&mut flat, errors, profiler)
    })?;
    // `translate` mints canonical FuncIds and stamps every call node at
    // construction ("born resolved"); there is no post-pass id assignment.
    Ok(in_span(profiler, "lower/translate", || {
        translate::translate(flat, plan)
    }))
}
