//! Resolved call-target identities.
//!
//! `CalleeRef` and `StaticMethodRef` bundle the `(module, name)` pairs that
//! identify a free function or a static method. Historically these were
//! threaded through the resolver as separate `&ModuleSource` + `&str`
//! parameters, which allowed the defining-module name and the caller-visible
//! alias to drift apart (see the bug where cross-module aliased generic calls
//! without turbofish failed: `lookup_generic_func_for_inference` re-looked up
//! a symbol by the defining-module name even though the symbol table is keyed
//! by the local alias).
//!
//! By forcing every downstream consumer to take `&CalleeRef` / `&StaticMethodRef`,
//! there is no way to accidentally split the pair, and the alias→defining-name
//! translation lives in exactly one place: the factory methods below. The
//! `name` fields are always the name *as defined in `module`*, never a local
//! alias.

use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::symbol::Symbol;

/// Identity of a free function callee: the module where it is defined and the
/// name under which it is defined in that module.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CalleeRef {
    pub module: ModuleSource,
    pub name: String,
}

impl CalleeRef {
    pub fn new(module: ModuleSource, name: impl Into<String>) -> Self {
        Self {
            module,
            name: name.into(),
        }
    }

    /// A callee defined in the current module (local function).
    pub fn local(current_module: &ModuleSource, name: impl Into<String>) -> Self {
        Self::new(current_module.clone(), name)
    }

    /// A callee imported into the current module via `use`. Translates the
    /// caller-visible alias (under which the symbol is keyed in the current
    /// module's symbol table) to the defining-module name in a single place.
    pub fn from_imported_symbol(symbol: &Symbol) -> Self {
        Self::new(symbol.module_source().clone(), symbol.name.clone())
    }

    /// A callee in `core:internal` (prelude functions like `panic`, `unreachable`).
    pub fn internal_prelude(name: impl Into<String>) -> Self {
        Self::new(ModuleSource::internal(), name)
    }

    /// A callee reached through a namespace-qualified call `Prefix::name`
    /// where `Prefix` is a module path (e.g. `Stdout::write`). The
    /// `prefix` is interned through the resolver's
    /// [`crate::module_source::ModuleSourceInterner`] and wrapped in a
    /// `ModuleSource::Local`.
    pub fn local_namespace(
        interner: &mut ModuleSourceInterner,
        prefix: &str,
        name: impl Into<String>,
    ) -> Self {
        Self::new(interner.local(prefix), name)
    }
}

/// Identity of a static method callee: the module of the `impl` block, the
/// receiver type name, and the method name. `trait_name` disambiguates when
/// the method comes from an `impl Trait for Type` block.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct StaticMethodRef {
    pub module: ModuleSource,
    pub type_name: String,
    pub method_name: String,
    pub trait_name: Option<String>,
}

impl StaticMethodRef {
    pub fn new(
        module: ModuleSource,
        type_name: impl Into<String>,
        method_name: impl Into<String>,
        trait_name: Option<String>,
    ) -> Self {
        Self {
            module,
            type_name: type_name.into(),
            method_name: method_name.into(),
            trait_name,
        }
    }
}
