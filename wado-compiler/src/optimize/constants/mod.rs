//! Constant optimization passes for Wado TIR.
//!
//! Groups four tightly-coupled passes that form a constant optimization pipeline:
//! 1. **Propagation**: replace `GlobalVarGet` of immutable globals with literal values
//! 2. **Folding**: evaluate compile-time operations (`2 + 3` → `5`)
//! 3. **Global promotion**: promote runtime globals back to compile-time when folded to constants
//! 4. **Branch pruning**: eliminate branches with known boolean conditions

mod branch_prune;
mod folding;
mod global_promotion;
mod propagation;

pub use branch_prune::prune_constant_branches;
pub use folding::fold_constants;
pub use global_promotion::promote_constant_globals;
pub use propagation::propagate_constants;
