//! Lower planner: gathers analysis facts and synthesized artefacts from
//! TIR before [`translate`](super::translate::translate) walks TIR → NIR.
//!
//! The planner is allowed to mutate the input [`FlatPackage`] when the
//! mutation is annotation-style (inserting `builtin::copy_value::<T>(...)`
//! markers, pushing synthesized helper functions, rewriting `Closure` /
//! `&primitive` / `IfLet` nodes in place). The translator is purely a
//! fold over TIR; everything that needs global analysis happens here.
//!
//! Sub-pass order, mirroring the previous in-place pipeline:
//!
//! 1. `pattern` — `IfLet` / `LetDestructure` → explicit `Let` + `If`;
//!    dense integer `Match` → `Switch`.
//! 2. `globals::extract` — extract non-constant initializers into
//!    `__initialize_<modname>` functions.
//! 3. `boxing` — `&primitive` / `&mut primitive` → `Box<T>` structs.
//! 4. `closure` — closures → `__Closure_N` functor structs with
//!    `__call` methods; returns `ClosurePlan`.
//! 5. `globals::build_initialize_modules` — combine per-module init
//!    functions into the top-level `__initialize_modules`.
//! 6. `value_copy` — insert `builtin::copy_value::<T>(x)` markers and
//!    synthesize `$value_copy$T<id>` helpers; returns `ValueCopyPlan`.
//! 7. `string` — collect literals and per-function DCE maps for the
//!    data section; returns `StringPlan`.
//!
//! Only the sub-passes that need to pass data through to the translator
//! carry a `*Plan` struct in [`LowerPlan`]; the others mutate `flat`
//! and return `()`.
//!
//! See `docs/wep-2026-05-11-nir.md`.

use crate::flat_package::FlatPackage;

pub mod boxing;
pub mod closure;
pub mod globals;
pub mod pattern;
pub mod string;
pub mod value_copy;

pub struct LowerPlan {
    pub closure: closure::ClosurePlan,
    pub value_copy: value_copy::ValueCopyPlan,
    pub strings: string::StringPlan,
}

pub fn plan(flat: &mut FlatPackage) -> LowerPlan {
    pattern::plan(flat);
    globals::extract(flat);
    boxing::plan(flat);
    let closure = closure::plan(flat);
    globals::build_initialize_modules(flat);
    flat.rebuild_variant_indices();
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
