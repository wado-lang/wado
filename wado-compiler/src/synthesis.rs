//! TIR synthesis phase.
//!
//! Generates synthetic TIR functions that cannot be expressed in user code:
//! 1. **Enum traits** — auto-derived `Eq` and `Ord` for enum types
//! 2. **Inspect** — debug output functions for `:?` format specifier
//! 3. **CM adapters** — Component Model boundary adapters for imports/exports
//!
//! Pipeline position: after `effect_check`, before `monomorphize`.

pub mod cm_adapter;
pub mod common;
pub mod enum_traits;
pub mod inspect;

use crate::project::Project;

/// Run all synthesis phases on the project.
///
/// Execution order:
/// 1. Enum traits — generates `Eq`/`Ord` for enums
/// 2. Inspect — synthesizes debug output functions
/// 3. CM adapters — generates Component Model boundary adapters
pub fn synthesize(project: Project) -> Result<Project, String> {
    let mut project = project;
    enum_traits::synthesize_enum_traits(&mut project);
    let project = inspect::synthesize_inspect(project);
    let project = cm_adapter::generate_adapters(project)?;
    Ok(project)
}
