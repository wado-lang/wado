//! Lower planner: gathers analysis facts and synthesized artefacts from
//! TIR before [`translate`](super::translate::translate) walks TIR → NIR.
//!
//! The planner is allowed to mutate the input [`FlatPackage`] when the
//! mutation is annotation-style (inserting `builtin::copy_value::<T>(...)`
//! markers, pushing synthesized helper functions, rewriting `Closure` /
//! `&primitive` / `IfLet` nodes in place). The translator is purely a
//! fold over TIR; everything that needs global analysis happens here.
//!
//! Sub-pass order:
//!
//! 1. `translate::pattern` — `IfLet` / `LetDestructure` → explicit
//!    `Let` + `Match`; or-pattern expansion, refutable sub-pattern
//!    extraction, and ref-deref on `Match` scrutinees. The
//!    implementation lives in the translator module because it
//!    builds the TIR shapes the translator consumes; it must run
//!    first so that downstream passes (boxing rewrites `&T` →
//!    `Box<T>`) don't obscure the `Ref` peeling pattern lowering
//!    needs. WEP 2026-05-11 Phase 10 Step 2b — moving this fully
//!    into the translator's fold — is blocked on Step 6 (boxing
//!    becoming analysis-only) for exactly this reason.
//! 2. `globals::extract` — extract non-constant initializers into a
//!    per-module `__initialize_module` function (one per source
//!    module; disambiguated by `module_source`).
//! 3. `boxing` — `&primitive` / `&mut primitive` → `Box<T>` structs.
//! 4. `closure` — closures → `__Closure_N` functor structs with
//!    `__call` methods; returns `ClosurePlan`.
//! 5. `globals::build_initialize_modules` — combine per-module init
//!    functions into the top-level `__initialize_modules`.
//! 6. `lift_mut::lift_mut_match_bindings` — lift `mut` payload
//!    bindings inside the `Match` arms produced by step 1 into
//!    explicit `Let mut` statements at the arm body start, the
//!    shape `value_copy::insert` keys on for its defensive-copy
//!    wrap — see [`lift_mut`] module docs.
//! 7. `value_copy` — insert `builtin::copy_value::<T>(x)` markers and
//!    synthesize `$value_copy$T<id>` helpers; returns `ValueCopyPlan`.
//! 8. `string` — collect literals and per-function DCE maps for the
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
pub mod lift_mut;
pub mod string;
pub mod value_copy;

pub struct LowerPlan {
    pub closure: closure::ClosurePlan,
    pub value_copy: value_copy::ValueCopyPlan,
    pub strings: string::StringPlan,
}

pub fn plan(flat: &mut FlatPackage) -> LowerPlan {
    super::translate::pattern::lower(flat);
    globals::extract(flat);
    boxing::plan(flat);
    let closure = closure::plan(flat);
    globals::build_initialize_modules(flat);
    flat.rebuild_variant_indices();
    // Lift `mut` payload bindings out of Match arm and IfLet patterns
    // into explicit `Let mut` statements before `value_copy` walks the
    // body. The lifted `Let mut` is the shape value_copy recognises;
    // doing it here keeps pattern lowering (step 1) free of value_copy
    // concerns — see [`lift_mut`] module docs.
    lift_mut::lift_mut_match_bindings(flat);
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
