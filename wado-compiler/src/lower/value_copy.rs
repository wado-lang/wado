//! Materialize Wado's value-copy semantics in TIR.
//!
//! This pair of passes runs at the end of the lower phase, after all other
//! TIR-shape transformations (boxing, closure lowering) and before the
//! optimizer. By emitting `$value_copy$T` calls before the optimizer loop,
//! every aliasing edge that downstream passes care about is explicit in
//! the TIR fed to the optimizer — `field_forward` and other flow-sensitive
//! analyses see a self-contained snapshot semantics.
//!
//! - `insert::insert_value_copy_calls` wraps every defensive deep-copy
//!   position in `builtin::copy_value::<T>(x)`.
//! - `synthesize::synthesize_value_copy_funcs` replaces those wrappers
//!   with calls to per-type `$value_copy$T_<id>` helper functions whose
//!   bodies perform a one-level shallow copy.
//!
//! Wrapper elision happens later in `optimize::value_copy_elide`, which
//! runs as a regular pass inside the optimizer fixed-point loop so that
//! newly-exposed `$value_copy$T(...)` patterns from inlining or SROA get
//! collapsed in the same iteration that exposes them.

pub mod insert;
pub mod synthesize;

pub use insert::insert_value_copy_calls;
pub use synthesize::synthesize_value_copy_funcs;
