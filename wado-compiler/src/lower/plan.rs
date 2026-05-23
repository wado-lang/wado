//! Lower planner: gathers analysis facts and synthesized artefacts from
//! TIR before [`translate`](super::translate::translate) walks TIR → NIR.
//!
//! After WEP 2026-05-11 Step 5 Phases A/B/C, the planner's allowed
//! mutations are confined to:
//!
//! - **Type-table changes** — `boxing::prepare_types` redefines
//!   `Ref` / `MutRef` `TypeId`s to `Box<T>` struct types.
//! - **Additive synthesis** — `globals::extract` /
//!   `build_initialize_modules`, the closure planner's
//!   `__Closure_N` structs and `__call` methods, and
//!   `value_copy::synthesize_helpers`'s `$value_copy$T` helpers.
//! - **Per-function declaration-shape edits** —
//!   `boxing::shadow_params` (parameter-shadow prelude `Let`s and
//!   non-param address-taken local retags) and
//!   `lift_mut::lift_mut_match_bindings` (pattern-bound `mut`
//!   payload lift). Both touch `func.locals` / `func.body` at the
//!   declaration level, not expression-shape rewrites.
//!
//! Every expression-shape rewrite (`&primitive` → `Box{...}`,
//! `Local(addr-taken) → .value`, `*box → .value`,
//! `Closure → StructLiteral`, value-copy wrap emission) happens in
//! the fold. Pattern lowering (`IfLet` / `LetDestructure` /
//! or-patterns → `Let` + `Match`) and string / bytes literal
//! collection likewise run inside [`translate`](super::translate),
//! *after* this planner finishes.
//!
//! Sub-pass order:
//!
//! 1. `globals::extract` — move non-constant initializers into
//!    per-module `__initialize_module` functions.
//! 2. `boxing::prepare_types` — mint `Box<T>` struct types and
//!    redefine `Ref` / `MutRef` `TypeId`s. Returns [`BoxPlan`].
//! 3. `boxing::shadow_params` — per-function: allocate `Box`-typed
//!    shadow locals for address-taken parameters, prepend prelude
//!    `Let`s, retag non-param address-taken locals.
//! 4. `closure::plan` — closures → `__Closure_N` functor structs
//!    with `__call` methods; analyse fn-param specialisation;
//!    returns [`ClosurePlan`].
//! 5. `globals::build_initialize_modules` — combine per-module init
//!    functions into the top-level `__initialize_modules`.
//! 6. `lift_mut::lift_mut_match_bindings` — lift `mut` payload
//!    bindings inside `Match` arm patterns into explicit `Let mut`
//!    statements at the arm body start. The lifted `Let mut` is
//!    the shape `value_copy::analyze` keys on for its
//!    defensive-copy wrap; see [`lift_mut`] module docs.
//! 7. `value_copy::plan` — read-only seed walk (the fold's wrap
//!    predicate applied at every wrap site) + transitive synthesis
//!    of `$value_copy$T` helpers; returns [`ValueCopyPlan`].
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
