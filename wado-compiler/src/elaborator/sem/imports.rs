//! [`ModuleImports`] — per-module name resolution context derived from
//! `use` declarations.
//!
//! Stage 1 skeleton introduced by
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]. Empty at this stage.
//!
//! # Membership rule
//!
//! Add a field here when it carries a fact derived from this module's
//! `use` declarations (or its local declarations that participate in
//! same-name resolution, like `interface` / `resource`). If the fact is
//! the canonicalisation of an *imported* name to its *declaring* module,
//! it belongs here. If the fact is a *local* declaration's body, it
//! belongs in [`super::decls::ModuleDecls`].
//!
//! # Planned contents
//!
//! - **`imported_type_sources`** — `local_name → defining ModuleSource`,
//!   from `use { Foo as Bar } from "..."`. Today on
//!   [`super::super::Elaborator::imported_type_sources`].
//! - **`import_original_names`** — `local_name → original_decl_name`, so
//!   aliased imports canonicalise to their original declaration name.
//!   Today on [`super::super::Elaborator::import_original_names`].
//! - **`namespace_imports`** — namespace-alias map (`use helper from "..."`).
//!   Today on [`super::super::Elaborator::namespace_imports`].
//! - **`effect_sources`** — effect name → module-source map built from
//!   import declarations and local `interface` / `resource` declarations.
//!   Today on [`super::super::Elaborator::effect_sources`].
//!
//! # Planned API
//!
//! - `lookup(local_name) -> Option<ModuleSource>`
//! - `canonical_decl_key(name) -> (ModuleSource, String)`
//! - effect-source map / namespace-alias map accessors.

pub(crate) struct ModuleImports {}
