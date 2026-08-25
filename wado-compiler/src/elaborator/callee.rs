//! Resolved call-target identities: a free function callee is the declaration
//! it names (WEP 2026-08-12), a static method callee what dispatch picked.

use crate::module_source::{ModuleSource, ModuleSourceInterner};

/// Identity of a free function callee. `Declared` carries the module and name
/// its one constructor reads off the table, so TIR emission needs none at hand;
/// only `def` says which declaration this is, and nothing reads a name back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum CalleeRef {
    Declared {
        def: crate::defs::DefId,
        module: ModuleSource,
        name: String,
    },
    /// A callee no declaration names in this currency: an effect operation,
    /// whose module is the namespace that signature resolution, the effect
    /// check, dispatch and WIR all key on rather than one any module declares.
    Rendered { module: ModuleSource, name: String },
}

impl CalleeRef {
    /// The free function `def` declares, rendered once from the table.
    pub fn declared(defs: &crate::defs::DefTable, def: crate::defs::DefId) -> Self {
        Self::Declared {
            def,
            module: defs.module(def).clone(),
            name: defs.name(def).to_string(),
        }
    }

    pub fn rendered(module: ModuleSource, name: impl Into<String>) -> Self {
        Self::Rendered {
            module,
            name: name.into(),
        }
    }

    /// A callee reached through `Prefix::name` where `Prefix` names an effect
    /// or resource rather than a module, so the prefix itself is the namespace.
    pub fn local_namespace(
        interner: &mut ModuleSourceInterner,
        prefix: &str,
        name: impl Into<String>,
    ) -> Self {
        Self::rendered(interner.local(prefix), name)
    }

    /// The declaration this names, or `None` for a [`Self::Rendered`] callee.
    pub fn def(&self) -> Option<crate::defs::DefId> {
        match self {
            Self::Declared { def, .. } => Some(*def),
            Self::Rendered { .. } => None,
        }
    }

    pub fn module(&self) -> &ModuleSource {
        match self {
            Self::Declared { module, .. } | Self::Rendered { module, .. } => module,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Declared { name, .. } | Self::Rendered { name, .. } => name,
        }
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
    pub trait_name: Option<crate::name::FqTraitName>,
    /// The method this selection picked. `None` when no declaration backs
    /// it — the auto-derived `Default::default`.
    pub method_id: Option<crate::defs::DefId>,
}

impl StaticMethodRef {
    pub fn new(
        module: ModuleSource,
        type_name: impl Into<String>,
        method_name: impl Into<String>,
        trait_name: Option<crate::name::FqTraitName>,
        method_id: Option<crate::defs::DefId>,
    ) -> Self {
        Self {
            module,
            type_name: type_name.into(),
            method_name: method_name.into(),
            trait_name,
            method_id,
        }
    }
}
