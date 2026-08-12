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
    /// The prelude's declarations, kept so [`Self::declaration_named`] performs
    /// the same lookup the walk did rather than a second one beside it.
    prelude: IndexMap<String, AstId>,
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
        Self {
            refs,
            decls,
            prelude,
        }
    }

    /// The declaration `name` means from `module`'s vantage, with no reference
    /// site to key on.
    ///
    /// Runs the same scope order the walk runs at every site, minus the
    /// binders — a caller holding a bare name is outside any item's type
    /// parameters. One lookup, so a name-only answer and the answer that name's
    /// site gets cannot differ.
    #[must_use]
    pub fn declaration_named(
        &self,
        module: &ModuleSource,
        name: &str,
        symbols: &SymbolTable,
    ) -> Option<(ModuleSource, String)> {
        module_scope_lookup(module, name, symbols, &self.prelude)
            .and_then(|id| symbols.get(&id))
            .map(|sym| (sym.module_source().clone(), sym.name.clone()))
    }

    /// The declaration a [`DeclRef`] names, for a consumer that already holds
    /// the answer rather than the site it came from.
    #[must_use]
    pub fn decl_named(&self, answer: DeclRef) -> Option<(&ModuleSource, &str)> {
        match answer {
            DeclRef::Decl(id) => self.decls.get(&id).map(|(m, n)| (m, n.as_str())),
            DeclRef::Binder(_) | DeclRef::Unresolved => None,
        }
    }

    /// The declaration a reference site names: the module that declares it and
    /// the name that declaration writes. `None` for a binder, a builtin shape,
    /// an unresolved name, or a site the walk never reached.
    #[must_use]
    pub fn declared(&self, site: AstId) -> Option<(&ModuleSource, &str)> {
        match self.get(site)? {
            DeclRef::Decl(id) => self.decls.get(&id).map(|(m, n)| (m, n.as_str())),
            DeclRef::Binder(_) | DeclRef::Unresolved => None,
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

/// Which declaration `name` reaches from `module`, ignoring type-parameter
/// binders: the module's own `use` imports, then its own declarations, then the
/// prelude's public surface, then the prelude's `internal` declarations.
///
/// The one implementation of the scope order, so the site walk and a name-only
/// caller cannot answer differently.
fn module_scope_lookup(
    module: &ModuleSource,
    name: &str,
    symbols: &SymbolTable,
    prelude: &IndexMap<String, AstId>,
) -> Option<AstId> {
    if let Some(sym) = symbols.imported(module, name) {
        return Some(sym.defined_at);
    }
    if let Some(sym) = symbols.lookup_in_module(module, name) {
        return Some(sym.defined_at);
    }
    if let Some(sym) = symbols.lookup_in_module(&ModuleSource::prelude(), name) {
        return Some(sym.defined_at);
    }
    prelude.get(name).copied()
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
    ///
    /// The module's own declarations rank above the prelude, so a module that
    /// declares `trait Left` means its own, not `core:prelude/format`'s enum
    /// case of that name (issue #1298).
    fn resolve_name(&self, name: &str) -> DeclRef {
        if let Some(id) = self.binder(name) {
            return DeclRef::Binder(id);
        }
        module_scope_lookup(self.module, name, self.symbols, self.prelude)
            .map_or(DeclRef::Unresolved, DeclRef::Decl)
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
        for p in params {
            self.visit_trait_bounds(&p.bounds);
            if let Some(default) = &p.default {
                self.visit_type(default);
            }
        }
    }

    /// A bound is a reference to a trait, and its associated-type bindings are
    /// references to that trait's members. Every bound position routes here —
    /// `<T: Trait>`, `trait Sub: Super`, `type A: Trait` — so an inherited
    /// supertrait bound carries a resolved site just like a written one.
    fn visit_trait_bounds(&mut self, bounds: &[ast::TraitBound]) {
        for bound in bounds {
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
    }

    /// A qualified path in expression position (`Trait::method`, `Type::CONST`)
    /// names a declaration with its leading segment, and that segment carries
    /// its own site. Without this the only trait reference a UFCS call has is
    /// a substring of the callee's name, which no vantage owns.
    fn visit_expr(&mut self, expr: &ast::Expr) {
        if let ast::Expr::Ident(ident) = expr
            && let [head, _rest @ ..] = ident.segments.as_slice()
            && ident.segments.len() > 1
        {
            let answer = self.resolve_name(&head.name);
            self.record(head.id, answer);
        }
        ast::walk_expr(self, expr);
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
                // `ns::Type` and `T::Assoc` both land here. A namespace import
                // registers each member under its `ns$member` alias, and that
                // is the only spelling naming the declaration behind
                // `ns::Type`. A binder namespace is an associated-type
                // projection, which names no declaration — and neither does a
                // namespace reaching no member. Resolving the bare member in
                // the writing module instead would confidently answer with a
                // different declaration that happens to share the name.
                let answer = match self.binder(&ns.namespace) {
                    Some(_) => DeclRef::Unresolved,
                    None => self
                        .symbols
                        .imported(
                            self.module,
                            &crate::name::namespace_member_alias(&ns.namespace, &ns.name),
                        )
                        .map_or(DeclRef::Unresolved, |sym| DeclRef::Decl(sym.defined_at)),
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
