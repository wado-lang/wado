//! Reference resolution: one answer per reference site.
//!
//! A type name in Wado source is module-relative, so which declaration a
//! spelling means is a fact about the module that wrote it. This pass answers
//! that question once, at the site, and records the answer under the site's own
//! [`AstId`]. Consumers read the table; none of them re-derives an identity from
//! a name, and none of them needs a module it may not have.
//!
//! See `docs/wep-2026-08-10-reference-resolution.md`.

use crate::ast::{self, AstId, AstVisitor, GenericParam, Item, Module, Type};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::symbol::SymbolTable;

/// A shape written without naming a declaration, so no module's scope decides
/// what it means. The named shapes are not here: `i32`, `()` and `!` are
/// `internal type` declarations in `core:prelude/primitive.wado` and resolve
/// like any other name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BuiltinShape {
    /// `[a, b]`
    Tuple,
    /// `&T` / `&mut T`
    Reference,
    /// `fn(..) -> ..`
    Function,
    /// `..T` in a variadic position.
    TypePack,
    /// `_`, or a node the parser already reported an error for.
    Placeholder,
}

/// What a reference site refers to.
///
/// Identity is the declaring node's [`AstId`] — the key [`SymbolTable`] is
/// already built on — so equality is declaration identity and there is no
/// spelling to compare instead.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DeclRef {
    /// A declaration, named by the node that declares it.
    Decl(AstId),
    /// A type parameter of an enclosing item, named by the parameter's own
    /// node. `Self` binds to the `impl` or `trait` that introduces it.
    Binder(AstId),
    /// A shape no module declares.
    Builtin(BuiltinShape),
    /// Reaches no declaration. Distinct from [`Self::Binder`] on purpose: a
    /// name that names nothing is not a type parameter, and reading it as one
    /// loses the diagnostic it deserves.
    Unresolved,
}

/// Every reference site's answer, keyed by the site's own [`AstId`].
#[derive(Debug, Default)]
pub struct Resolutions {
    refs: IndexMap<AstId, DeclRef>,
    /// Every declaration a reference reaches, by its declaring node's `AstId`.
    /// Carried here so a consumer can name what a site resolved to without
    /// holding the `SymbolTable` — the table is the one place both are known.
    decls: IndexMap<AstId, (ModuleSource, String)>,
}

impl Resolutions {
    /// Resolve every reference site in every loaded module, each from the
    /// module that wrote it.
    pub fn build(modules: &IndexMap<ModuleSource, Module>, symbols: &SymbolTable) -> Self {
        let prelude = prelude_declarations(symbols);
        let mut refs = IndexMap::default();
        for (module_source, module) in modules {
            let mut resolver = Resolver {
                module: module_source,
                symbols,
                binders: Vec::new(),
                prelude: &prelude,
                refs: &mut refs,
            };
            for item in &module.items {
                resolver.visit_item(item);
            }
        }
        let mut decls: IndexMap<AstId, (ModuleSource, String)> = IndexMap::default();
        for answer in refs.values() {
            if let DeclRef::Decl(id) = answer
                && let Some(sym) = symbols.get(id)
            {
                decls.insert(*id, (sym.module_source().clone(), sym.name.clone()));
            }
        }
        Self { refs, decls }
    }

    /// The declaration a [`DeclRef`] names, for a consumer that already holds
    /// the answer rather than the site it came from.
    #[must_use]
    pub fn decl_named(&self, answer: DeclRef) -> Option<(&ModuleSource, &str)> {
        match answer {
            DeclRef::Decl(id) => self.decls.get(&id).map(|(m, n)| (m, n.as_str())),
            DeclRef::Binder(_) | DeclRef::Builtin(_) | DeclRef::Unresolved => None,
        }
    }

    /// The declaration a reference site names: the module that declares it and
    /// the name that declaration writes. `None` for a binder, a builtin shape,
    /// an unresolved name, or a site the walk never reached.
    #[must_use]
    pub fn declared(&self, site: AstId) -> Option<(&ModuleSource, &str)> {
        match self.get(site)? {
            DeclRef::Decl(id) => self.decls.get(&id).map(|(m, n)| (m, n.as_str())),
            DeclRef::Binder(_) | DeclRef::Builtin(_) | DeclRef::Unresolved => None,
        }
    }

    /// The answer for a reference site, or `None` when the site was never
    /// walked — a coverage hole rather than an unresolved name.
    #[must_use]
    pub fn get(&self, site: AstId) -> Option<DeclRef> {
        self.refs.get(&site).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.refs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.refs.is_empty()
    }

    /// How many sites landed in each answer, for the shadow-stage census.
    #[must_use]
    pub fn census(&self) -> Census {
        let mut c = Census::default();
        for r in self.refs.values() {
            match r {
                DeclRef::Decl(_) => c.decl += 1,
                DeclRef::Binder(_) => c.binder += 1,
                DeclRef::Builtin(_) => c.builtin += 1,
                DeclRef::Unresolved => c.unresolved += 1,
            }
        }
        c
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Census {
    pub decl: usize,
    pub binder: usize,
    pub builtin: usize,
    pub unresolved: usize,
}

/// The name `Self` binds to inside a `trait` or `impl` body.
const SELF_TYPE: &str = "Self";

/// Every declaration in the prelude and its implementation modules, by name.
///
/// The prelude is in scope everywhere without a `use`, and `internal`
/// visibility governs what a user may *implement*, not whether the compiler can
/// tell which declaration a name means — an `impl ReflectStruct for T` has to
/// resolve before it can be rejected as sealed. First writer wins, matching the
/// load order the rest of the compiler sees.
fn prelude_declarations(symbols: &SymbolTable) -> IndexMap<String, AstId> {
    let mut out: IndexMap<String, AstId> = IndexMap::default();
    for (id, sym) in symbols.iter() {
        if is_prelude_module(sym.module_source()) {
            out.entry(sym.name.clone()).or_insert(*id);
        }
    }
    out
}

fn is_prelude_module(module: &ModuleSource) -> bool {
    matches!(module, ModuleSource::Core { name } if name.as_str() == "prelude"
        || name.as_str().starts_with("prelude/"))
}

struct Resolver<'a> {
    /// The vantage: the module every name in this walk is written in.
    module: &'a ModuleSource,
    symbols: &'a SymbolTable,
    /// Binders in scope, innermost last. A name found here is the enclosing
    /// item's parameter and no module scope is consulted for it.
    binders: Vec<IndexMap<String, AstId>>,
    /// The prelude's declarations, including the `internal` ones its
    /// implementation modules keep to themselves. Every module sees these
    /// without a `use`, which is what makes `i32` and `List` universal.
    prelude: &'a IndexMap<String, AstId>,
    refs: &'a mut IndexMap<AstId, DeclRef>,
}

impl Resolver<'_> {
    fn binder(&self, name: &str) -> Option<AstId> {
        self.binders
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    /// The declaration a name written in this module refers to.
    ///
    /// The layers are ordered, and the order is the rule: the enclosing item's
    /// binders, this module's explicit imports (keyed by the local name, so an
    /// alias resolves to what it aliases), this module's own declarations, then
    /// the prelude — including its implementation modules, so `i32`, `List` and
    /// a compiler item declared `internal` there (`ReflectStruct`, `Member`)
    /// all resolve for a module that never `use`d them.
    fn resolve_name(&self, name: &str) -> DeclRef {
        if let Some(id) = self.binder(name) {
            return DeclRef::Binder(id);
        }
        if let Some(sym) = self.symbols.lookup(self.module, name) {
            return DeclRef::Decl(sym.defined_at);
        }
        if let Some(sym) = self.symbols.lookup_in_module(self.module, name) {
            return DeclRef::Decl(sym.defined_at);
        }
        if let Some(id) = self.prelude.get(name) {
            return DeclRef::Decl(*id);
        }
        DeclRef::Unresolved
    }

    fn record(&mut self, site: AstId, answer: DeclRef) {
        self.refs.insert(site, answer);
    }

    fn push_binders(&mut self, params: &[GenericParam], self_binder: Option<AstId>) {
        let mut scope: IndexMap<String, AstId> = IndexMap::default();
        if let Some(id) = self_binder {
            scope.insert(SELF_TYPE.to_string(), id);
        }
        for p in params {
            scope.insert(p.name.clone(), p.id);
        }
        self.binders.push(scope);
    }

    fn in_scope(
        &mut self,
        params: &[GenericParam],
        self_binder: Option<AstId>,
        walk: impl FnOnce(&mut Self),
    ) {
        self.push_binders(params, self_binder);
        walk(self);
        self.binders.pop();
    }
}

impl AstVisitor for Resolver<'_> {
    fn visit_item(&mut self, item: &Item) {
        // Every item that introduces type parameters opens a binder scope for
        // the whole of its body, so a name inside it is checked against them
        // before any module scope.
        let (params, self_binder): (&[GenericParam], Option<AstId>) = match item {
            Item::Struct(s) => (&s.type_params, None),
            Item::Enum(e) => (&e.type_params, None),
            Item::Variant(v) => (&v.type_params, None),
            Item::Newtype(n) => (&n.type_params, None),
            Item::BuiltinTypeDecl(d) => (&d.type_params, None),
            Item::Resource(r) => (&r.type_params, Some(r.id)),
            Item::Impl(i) => (&i.type_params, Some(i.id)),
            Item::Trait(t) => (&t.type_params, Some(t.id)),
            Item::Function(_)
            | Item::Interface(_)
            | Item::Flags(_)
            | Item::TupleTypeDecl(_)
            | Item::World(_)
            | Item::Test(_)
            | Item::Global(_)
            | Item::Use(_)
            | Item::Error(_) => (&[], None),
        };
        self.in_scope(params, self_binder, |s| ast::walk_item(s, item));
    }

    fn visit_function(&mut self, func: &ast::Function) {
        self.in_scope(&func.type_params, None, |s| ast::walk_function(s, func));
    }

    fn visit_generic_params(&mut self, params: &[GenericParam]) {
        // A bound is a reference to a trait, and its associated-type bindings
        // are references to that trait's members.
        for p in params {
            for bound in &p.bounds {
                let answer = self.resolve_name(&bound.name);
                self.record(bound.id, answer);
                for assoc in &bound.assoc_types {
                    // The member is named relative to the bound's trait, not to
                    // this module, so the site is recorded and left for the
                    // consumer that knows the trait.
                    self.record(assoc.id, DeclRef::Unresolved);
                    self.visit_type(&assoc.ty);
                }
            }
            if let Some(default) = &p.default {
                self.visit_type(default);
            }
        }
    }

    fn visit_type(&mut self, ty: &Type) {
        match ty {
            Type::Named(named) => {
                let answer = self.resolve_name(&named.name);
                self.record(named.id, answer);
            }
            Type::Generic(generic) => {
                let answer = self.resolve_name(&generic.name);
                self.record(generic.id, answer);
                for arg in &generic.args {
                    self.visit_type(arg);
                }
            }
            Type::NamespacedGeneric(ns) => {
                // `ns::Type` and `T::Assoc` both land here: the namespace is
                // either an import alias or a binder, and only the former names
                // a module whose declaration this is.
                let answer = match self.binder(&ns.namespace) {
                    Some(_) => DeclRef::Unresolved,
                    None => self.resolve_name(&ns.name),
                };
                self.record(ns.id, answer);
                for arg in &ns.args {
                    self.visit_type(arg);
                }
            }
            Type::Tuple(elems) => {
                for e in elems {
                    self.visit_type(e);
                }
            }
            Type::Reference(inner) | Type::MutReference(inner) => self.visit_type(inner),
            Type::Function(ft) => {
                for p in &ft.params {
                    self.visit_type(p);
                }
                self.visit_type(&ft.return_type);
            }
            Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => {}
        }
    }
}

/// The reference site a type's head is written at, or `None` for a shape that
/// names no declaration. References are fundamental, so `&T` asks about `T`.
#[must_use]
pub fn head_site(ty: &Type) -> Option<AstId> {
    match ty {
        Type::Named(named) => Some(named.id),
        Type::Generic(generic) => Some(generic.id),
        Type::NamespacedGeneric(ns) => Some(ns.id),
        Type::Reference(inner) | Type::MutReference(inner) => head_site(inner),
        Type::Tuple(_)
        | Type::Function(_)
        | Type::TypePackSpread(..)
        | Type::Infer(_)
        | Type::Error(_) => None,
    }
}

/// One site where the table and a consumer's own derivation disagree.
#[derive(Debug)]
pub struct Disagreement {
    pub module: ModuleSource,
    pub written: String,
    pub table: DeclRef,
    /// What the consumer derived, rendered — it has no identity to compare.
    pub consumer: String,
}

impl Resolutions {
    /// The declaration a site resolves to, as the `(module, name)` pair the
    /// consumers still key on. Shadow-stage only: it exists to compare the
    /// table against a consumer's own derivation, and goes when the consumers
    /// take a `DeclRef`.
    #[must_use]
    pub fn decl_key(&self, site: AstId, symbols: &SymbolTable) -> Option<(ModuleSource, String)> {
        match self.get(site)? {
            DeclRef::Decl(id) => symbols
                .get(&id)
                .map(|sym| (sym.module_source().clone(), sym.name.clone())),
            DeclRef::Binder(_) | DeclRef::Builtin(_) | DeclRef::Unresolved => None,
        }
    }

    /// Whether shadow reporting is on. The stage-B instrument: it measures how
    /// far the table is from what the consumers derive today, before anything
    /// depends on it.
    #[must_use]
    pub fn shadow_enabled() -> bool {
        std::env::var("WADO_RESOLVE_SHADOW").is_ok()
    }

    /// Report a disagreement to stderr, prefixed so a whole-suite run can be
    /// grepped for them.
    pub fn report(d: &Disagreement) {
        eprintln!(
            "resolve-shadow: {} wrote `{}`: table={:?} consumer={}",
            d.module, d.written, d.table, d.consumer
        );
    }
}
