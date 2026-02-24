//! TIR synthesis phase.
//!
//! Generates synthetic TIR functions that cannot be expressed in user code:
//! 1. **Enum traits** — auto-derived `Eq` and `Ord` for enum types
//! 2. **Inspect** — debug output functions for `:?` format specifier
//! 3. **CM adapters** — Component Model boundary adapters for imports/exports
//!
//! Inspect synthesis runs in two passes:
//! - **Pre-monomorphize** (here): handles concrete types, generates inspect functions
//!   that go through monomorphization for proper generic method resolution.
//! - **Post-monomorphize** (`inspect::synthesize_inspect` called from `lib.rs`):
//!   handles deferred markers from generic type parameters (`TypeParam`),
//!   which are now concrete types after monomorphization. Also resolves
//!   `builtin::display` markers with correct Display vs Inspect dispatch.

pub mod cm_adapter;
pub mod common;
pub mod inspect;
pub mod traits;

use crate::project::Project;

/// Run pre-monomorphize synthesis phases on the project.
///
/// Execution order:
/// 1. Traits — generates `Eq`/`Ord` for enums
/// 2. Inspect — synthesizes debug output functions (concrete types only;
///    `TypeParam` markers are deferred to the post-monomorphize pass)
/// 3. CM adapters — generates Component Model boundary adapters
pub fn synthesize(project: Project) -> Result<Project, String> {
    let project = traits::synthesize_traits(project);
    let project = inspect::synthesize_inspect(project);
    let project = cm_adapter::generate_adapters(project)?;
    Ok(project)
}
