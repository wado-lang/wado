//! Lower planner: gathers analysis facts and synthesized artefacts from
//! TIR before [`translate`](super::translate::translate) walks TIR → NIR.
//!
//! The planner is allowed to mutate the input [`FlatPackage`] when the
//! mutation is annotation-style (e.g. inserting `builtin::copy_value::<T>(...)`
//! markers, pushing synthesized helper functions). The translator is
//! purely a fold over TIR; everything that needs global analysis
//! happens here.
//!
//! See `docs/wep-2026-05-11-nir.md` for the broader NIR migration.

use crate::flat_package::FlatPackage;

pub mod closure;
pub mod string;
pub mod value_copy;

pub struct LowerPlan {
    pub closure: closure::ClosurePlan,
    pub value_copy: value_copy::ValueCopyPlan,
    pub strings: string::StringPlan,
}

pub fn plan(flat: &mut FlatPackage) -> LowerPlan {
    // Closure runs first: it rewrites `Closure` / `FuncRef` /
    // `Capture` / `IndirectCall` nodes and registers `__Closure_N`
    // structs and `__call` methods that `value_copy` and `strings`
    // walk over.
    let closure = closure::plan(flat);
    let value_copy = value_copy::plan(flat);
    // Strings are collected after `value_copy` planning so any literals
    // that synthesized `$value_copy$T<id>` helpers introduce are
    // included.
    let strings = string::plan(flat);
    LowerPlan {
        closure,
        value_copy,
        strings,
    }
}
