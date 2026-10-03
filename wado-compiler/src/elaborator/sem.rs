//! [`ModuleSemantics`] — per-module semantic facts (WEP 2026-05-26), one
//! instance per loaded module. A module a compile walks owns its instance, which
//! the per-module [`super::Elaborator`] takes for the length of one pass; a
//! stdlib module served from the snapshot shares the snapshot's, which no pass
//! takes. Every other phase borrows. Membership
//! splits across four sub-structs, each in its own file so the "does this fit?"
//! question that gates a new field stays reviewable.

pub mod bindings;
pub mod decls;
pub mod imports;
pub mod types;

pub use bindings::ModuleBindings;
pub use decls::ModuleDecls;
pub use imports::ModuleImports;
pub use types::TypeAnnotations;

use crate::ast::AstId;
use crate::hashmap::IndexMap;
use crate::symbol::Symbol;
use crate::tir::TypeId;

/// Per-module semantic facts. See the module-level documentation for the
/// membership rules and ownership story.
#[derive(Default, Clone)]
pub struct ModuleSemantics {
    pub bindings: ModuleBindings,
    pub imports: ModuleImports,
    pub types: TypeAnnotations,
    pub decls: ModuleDecls,
    /// Per-impl trait default-method facts, keyed by
    /// `(impl_block.id, trait_default_method.ast_id)`. The same trait body is
    /// synthesised once per impl, so one trait node legitimately carries a fact
    /// set per impl, which a flat map could not hold.
    pub default_method_facts: IndexMap<(AstId, AstId), DefaultMethodFacts>,
}

/// What one walk of a trait default body records for one impl. Its
/// declarations and imports are the impl module's, so it holds none.
#[derive(Clone)]
pub struct DefaultMethodFacts {
    pub bindings: ModuleBindings,
    pub types: TypeAnnotations,
}

/// The fact kinds [`crate::semantics::Semantics`] answers by `AstId`, and so
/// routes to a module by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FactKind {
    Reference,
    LocalSymbol,
    LocalType,
    ExpressionType,
    MethodDispatch,
    Coercion,
    Desugar,
}

/// Where a routed fact lives: a binding is recorded once per module, a body
/// fact once per walk that reached the node — the module's own and one per
/// tuple `for-of` element ([`types::TypeAnnotations::all`]).
pub enum FactMap<V: 'static> {
    Bindings(fn(&ModuleBindings) -> &IndexMap<AstId, V>),
    Body(fn(&types::BodyFacts) -> &IndexMap<AstId, V>),
}

/// A routed fact kind and the map it lives in, paired once: a query names a
/// [`Fact`], so it cannot route by one kind and read another.
pub struct Fact<V: 'static> {
    pub kind: FactKind,
    map: FactMap<V>,
}

// Hand-written so the copy does not demand `V: Copy`: a `Fact` is a kind and
// a function pointer whatever it maps to.
impl<V> Clone for FactMap<V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V> Copy for FactMap<V> {}
impl<V> Clone for Fact<V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V> Copy for Fact<V> {}

impl<V> Fact<V> {
    const fn new(kind: FactKind, map: FactMap<V>) -> Self {
        Self { kind, map }
    }

    /// Every `(site, value)` this module's walks recorded, a site repeated once
    /// per walk that reached it.
    pub fn entries(self, sem: &ModuleSemantics) -> impl Iterator<Item = (AstId, &V)> {
        let bindings = match self.map {
            FactMap::Bindings(map) => Some(map(&sem.bindings).iter()),
            FactMap::Body(_) => None,
        };
        let body = match self.map {
            FactMap::Bindings(_) => None,
            FactMap::Body(map) => Some(sem.types.walks().flat_map(move |facts| map(facts).iter())),
        };
        bindings
            .into_iter()
            .flatten()
            .chain(body.into_iter().flatten())
            .map(|(id, value)| (*id, value))
    }

    /// Every value recorded for `id`: at most one for a binding, and for a body
    /// fact the module's own walk's first, then one per tuple `for-of` element.
    pub fn all(self, sem: &ModuleSemantics, id: AstId) -> impl Iterator<Item = &V> {
        let binding = match self.map {
            FactMap::Bindings(map) => map(&sem.bindings).get(&id),
            FactMap::Body(_) => None,
        };
        let body = match self.map {
            FactMap::Bindings(_) => None,
            FactMap::Body(map) => Some(sem.types.all(map, id)),
        };
        binding.into_iter().chain(body.into_iter().flatten())
    }

    fn keys(self, sem: &ModuleSemantics) -> impl Iterator<Item = (AstId, FactKind)> + '_ {
        self.entries(sem).map(move |(id, _)| (id, self.kind))
    }
}

impl ModuleSemantics {
    pub const REFERENCES: Fact<AstId> =
        Fact::new(FactKind::Reference, FactMap::Bindings(|b| &b.references));
    pub const LOCAL_SYMBOLS: Fact<Symbol> = Fact::new(
        FactKind::LocalSymbol,
        FactMap::Bindings(|b| &b.local_symbols),
    );
    pub const LOCAL_TYPES: Fact<TypeId> =
        Fact::new(FactKind::LocalType, FactMap::Body(|t| &t.local_types));
    pub const EXPRESSION_TYPES: Fact<TypeId> = Fact::new(
        FactKind::ExpressionType,
        FactMap::Body(|t| &t.expression_types),
    );
    pub const METHOD_DISPATCH: Fact<types::MethodDispatch> = Fact::new(
        FactKind::MethodDispatch,
        FactMap::Body(|t| &t.method_dispatch),
    );
    pub const COERCIONS: Fact<types::CoercionChoice> =
        Fact::new(FactKind::Coercion, FactMap::Body(|t| &t.coercions));
    pub const DESUGARS: Fact<types::DesugarKind> =
        Fact::new(FactKind::Desugar, FactMap::Body(|t| &t.desugars));

    /// Every fact this module's walk recorded that `Semantics` answers from —
    /// the list its routing is built over.
    pub fn routed_facts(&self) -> impl Iterator<Item = (AstId, FactKind)> + '_ {
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
    pub fn fact_count(&self) -> usize {
        self.bindings.references.len() + self.bindings.local_symbols.len() + self.types.fact_count()
    }
}
