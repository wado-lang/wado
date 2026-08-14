//! Lowering `FlatPackage` → `NirPackage` (WEP 2026-05-11), as planner then
//! translator. [`plan::plan`] runs the TIR-mutating sub-passes — pattern
//! lowering, global-initializer extraction, boxing, closure lowering, value-copy
//! materialization, string-literal collection — and hands the translator a
//! [`plan::LowerPlan`]. [`translate::translate`] is then a single fold.

pub mod bare_asserts;
pub mod plan;
pub mod translate;
pub(crate) mod wide_int_literal;

use crate::flat_package::FlatPackage;
use crate::logger::{Bail, ErrorSink};
use crate::nir_package::NirPackage;

/// Lower a [`FlatPackage`] and return a [`NirPackage`].
pub fn lower(mut flat: FlatPackage, errors: &dyn ErrorSink) -> Result<NirPackage, Bail> {
    let plan = plan::plan(&mut flat, errors)?;
    // `translate` mints canonical FuncIds and stamps every call node at
    // construction ("born resolved"); there is no post-pass id assignment.
    Ok(translate::translate(flat, plan))
}
