//! Lower planner: gathers analysis facts and synthesized artefacts from
//! TIR before [`translate`](super::translate::translate) walks TIR → NIR.
//!
//! The planner is allowed to mutate the input [`FlatPackage`] when the
//! mutation is annotation-style (inserting `builtin::copy_value::<T>(...)`
//! markers, pushing synthesized helper functions, rewriting `Closure` /
//! `&primitive` nodes in place). The translator is purely a fold over
//! TIR; everything that needs global analysis happens here.
//!
//! Pattern lowering (`IfLet` / `LetDestructure` / or-patterns →
//! explicit `Let` + `Match`) is no longer a `plan` sub-pass: it runs
//! at the start of [`translate`](super::translate::translate), before
//! the fold (WEP 2026-05-11 Phase 10 Step 2b — see
//! [`translate::pattern`](super::translate::pattern)). String/bytes
//! literal collection (`string`) likewise runs inside `translate`,
//! *after* pattern lowering — pattern lowering synthesises
//! string-literal expressions (string-literal pattern guards) that
//! the data section must register.
//!
//! Sub-pass order:
//!
//! 1. `globals::extract` — extract non-constant initializers into a
//!    per-module `__initialize_module` function (one per source
//!    module; disambiguated by `module_source`).
//! 2. `boxing::prepare_types` — mint `Box<T>` struct types and
//!    redefine `Ref` / `MutRef` `TypeIds`; `boxing::lower_bodies` —
//!    rewrite `&primitive` / `&mut primitive` into `Box<T>` struct
//!    operations.
//! 3. `closure` — closures → `__Closure_N` functor structs with
//!    `__call` methods; returns `ClosurePlan`.
//! 4. `globals::build_initialize_modules` — combine per-module init
//!    functions into the top-level `__initialize_modules`.
//! 5. `lift_mut::lift_mut_match_bindings` — lift `mut` payload
//!    bindings inside `Match` arm and `IfLet` patterns into explicit
//!    `Let mut` statements at the arm body / then-block start, the
//!    shape `value_copy::insert` keys on for its defensive-copy
//!    wrap — see [`lift_mut`] module docs.
//! 6. `value_copy` — insert `builtin::copy_value::<T>(x)` markers and
//!    synthesize `$value_copy$T<id>` helpers; returns `ValueCopyPlan`.
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
    pub box_plan: boxing::BoxPlan,
    pub closure: closure::ClosurePlan,
    pub value_copy: value_copy::ValueCopyPlan,
}

pub fn plan(flat: &mut FlatPackage) -> LowerPlan {
    globals::extract(flat);
    let box_plan = boxing::prepare_types(flat);
    // Box-aware parameter shadowing must happen before closure /
    // `value_copy` planning so the synthesized prelude `Let`s and the
    // updated `address_taken_locals` set are visible. Body-level
    // boxing rewrites (`&primitive → Box{...}`, `Local → .value`,
    // `*box → .value`, deref-assign struct expansion) now run in the
    // fold; see `lower::translate`.
    boxing::shadow_params(flat, &box_plan);
    let closure = closure::plan(flat);
    globals::build_initialize_modules(flat);
    flat.rebuild_variant_indices();
    // Lift `mut` payload bindings out of Match arm and IfLet patterns
    // into explicit `Let mut` statements before `value_copy` walks the
    // body. The lifted `Let mut` is the shape value_copy recognises.
    lift_mut::lift_mut_match_bindings(flat);
    let value_copy = value_copy::plan(flat);
    LowerPlan {
        box_plan,
        closure,
        value_copy,
    }
}
