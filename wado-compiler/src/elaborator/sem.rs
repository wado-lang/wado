//! [`ModuleSemantics`] — per-module semantic facts (WEP 2026-05-26), one
//! instance per loaded module that the per-module [`super::Elaborator`] takes
//! for the length of one pass over that module and every other phase borrows.
//! Membership
//! splits across four sub-structs, each in its own file so the "does this fit?"
//! question that gates a new field stays reviewable.

pub(crate) mod bindings;
pub(crate) mod decls;
pub(crate) mod imports;
pub(crate) mod types;

pub(crate) use bindings::ModuleBindings;
pub(crate) use decls::ModuleDecls;
pub(crate) use imports::ModuleImports;
pub(crate) use types::TypeAnnotations;

use crate::ast::AstId;
use crate::hashmap::IndexMap;
use crate::symbol::Symbol;
use crate::tir::TypeId;

/// Per-module semantic facts. See the module-level documentation for the
/// membership rules and ownership story.
#[derive(Default, Clone)]
pub(crate) struct ModuleSemantics {
    pub(crate) bindings: ModuleBindings,
    pub(crate) imports: ModuleImports,
    pub(crate) types: TypeAnnotations,
    pub(crate) decls: ModuleDecls,
    /// Per-impl trait default-method `ModuleSemantics` snapshots, keyed by
    /// `(impl_block.id, trait_default_method.ast_id)`. The same trait body is
    /// synthesised once per impl, so one trait node legitimately carries a fact
    /// set per impl, which snapshot isolation gives and a flat map could not.
    /// Each value's `decls` / `imports` are cloned from the impl module.
    pub(crate) default_method_semantics: IndexMap<(AstId, AstId), ModuleSemantics>,
}

/// The fact kinds [`crate::semantics::Semantics`] answers by `AstId`, and so
/// routes to a module by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum FactKind {
    Reference,
    LocalSymbol,
    LocalType,
    ExpressionType,
    MethodDispatch,
    Coercion,
    Desugar,
}

/// A routed fact kind and the map it lives in, paired once: a query names a
/// [`Fact`], so it cannot route by one kind and read another.
#[derive(Clone, Copy)]
pub(crate) struct Fact<V: 'static> {
    pub(crate) kind: FactKind,
    pub(crate) map: fn(&ModuleSemantics) -> &IndexMap<AstId, V>,
}

impl<V> Fact<V> {
    const fn new(kind: FactKind, map: fn(&ModuleSemantics) -> &IndexMap<AstId, V>) -> Self {
        Self { kind, map }
    }

    fn keys(self, sem: &ModuleSemantics) -> impl Iterator<Item = (AstId, FactKind)> + '_ {
        (self.map)(sem).keys().map(move |id| (*id, self.kind))
    }
}

impl ModuleSemantics {
    pub(crate) const REFERENCES: Fact<AstId> =
        Fact::new(FactKind::Reference, |sem| &sem.bindings.references);
    pub(crate) const LOCAL_SYMBOLS: Fact<Symbol> =
        Fact::new(FactKind::LocalSymbol, |sem| &sem.bindings.local_symbols);
    pub(crate) const LOCAL_TYPES: Fact<TypeId> =
        Fact::new(FactKind::LocalType, |sem| &sem.types.local_types);
    pub(crate) const EXPRESSION_TYPES: Fact<TypeId> =
        Fact::new(FactKind::ExpressionType, |sem| &sem.types.expression_types);
    pub(crate) const METHOD_DISPATCH: Fact<types::MethodDispatch> =
        Fact::new(FactKind::MethodDispatch, |sem| &sem.types.method_dispatch);
    pub(crate) const COERCIONS: Fact<types::CoercionChoice> =
        Fact::new(FactKind::Coercion, |sem| &sem.types.coercions);
    pub(crate) const DESUGARS: Fact<types::DesugarKind> =
        Fact::new(FactKind::Desugar, |sem| &sem.types.desugars);

    /// Every fact this module's walk recorded that `Semantics` answers from —
    /// the list its routing is built over.
    pub(crate) fn routed_facts(&self) -> impl Iterator<Item = (AstId, FactKind)> + '_ {
        Self::REFERENCES
            .keys(self)
            .chain(Self::LOCAL_SYMBOLS.keys(self))
            .chain(Self::LOCAL_TYPES.keys(self))
            .chain(Self::EXPRESSION_TYPES.keys(self))
            .chain(Self::METHOD_DISPATCH.keys(self))
            .chain(Self::COERCIONS.keys(self))
            .chain(Self::DESUGARS.keys(self))
    }
}

#[cfg(debug_assertions)]
impl ModuleSemantics {
    /// How many facts have been recorded, for the guard that a *query* left no
    /// trace (`Elaborator::synthesize_arg_class`). A count suffices: every
    /// recording grows one of these maps.
    pub(crate) fn fact_count(&self) -> usize {
        let b = &self.bindings;
        let t = &self.types;
        b.references.len()
            + b.local_symbols.len()
            + t.local_types.len()
            + t.expression_types.len()
            + t.method_dispatch.len()
            + t.coercions.len()
            + t.desugars.len()
            + t.generic_instantiations.len()
            + t.closure_captures.len()
            + t.call_param_types.len()
            + t.assert_captures.len()
            + t.for_of_iterator.len()
            + t.operator_dispatch.len()
            + t.handler_bindings.len()
            + t.impl_facts.len()
            + t.function_effects.len()
            + t.function_task_returns.len()
            + t.static_method_dispatch.len()
            + t.sequence_coercions.len()
            + t.key_value_coercions.len()
            + t.literal_conversions.len()
            + t.from_call_facts.len()
            + t.index_assign_dispatch.len()
            + t.tuple_overlays.len()
            + t.method_impl_type_params.len()
            + t.fn_param_types.len()
            + t.fn_return_types.len()
            + t.effect_ops.len()
            + t.decl_type_params.len()
            + t.method_names.len()
            + t.let_annotated_types.len()
            + t.struct_field_types.len()
            + t.assign_places.len()
            + t.bare_cases.len()
    }
}
