//! Reference resolution: one answer per reference site.
//!
//! A name in Wado source is module-relative, so which declaration a spelling
//! means is a fact about the module that wrote it. This pass answers that
//! question once, at the site, and records the answer under the site's own
//! [`AstId`]. Consumers read the table; none of them re-derives an identity from
//! a name, and none of them needs a module it may not have.
//!
//! This is the only place a name becomes a [`DefId`]: [`Scope`] is private here,
//! and [`crate::defs::DefTable`] has no name-keyed lookup of its own.
//!
//! See `docs/wep-2026-08-12-declaration-identity.md`.

use std::sync::Arc;

use crate::ast::{self, AstId, AstVisitor, GenericParam, Item, Module, Type};
use crate::defs::{DefId, DefTable};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::symbol::SymbolTable;

/// What a reference site refers to.
///
/// The three cases stay distinct on purpose: reading [`Self::Unresolved`] as a
/// binder loses the diagnostic a name that reaches nothing deserves.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Resolution {
    /// A declaration.
    Def(DefId),
    /// A type parameter of an enclosing item, named by the parameter's own
    /// node. `Self` binds to the `impl` or `trait` that introduces it.
    Binder(AstId),
    /// Reaches no declaration.
    Unresolved,
}

impl Resolution {
    /// The declaration this answer names, or `None` for a binder and for a name
    /// that reaches nothing.
    #[must_use]
    pub fn def(self) -> Option<DefId> {
        match self {
            Self::Def(def) => Some(def),
            Self::Binder(_) | Self::Unresolved => None,
        }
    }
}

/// Every reference site's answer, keyed by the site's own [`AstId`].
#[derive(Debug)]
pub struct Resolutions {
    /// Every declaration in the program. Carried here because this pass is what
    /// produces identities, and a consumer reading an answer needs the table to
    /// render it.
    defs: Arc<DefTable>,
    refs: IndexMap<AstId, Resolution>,
    /// The module scopes, layered. Held here rather than recomputed, so the
    /// site walk and a name-only caller run one implementation and cannot
    /// answer differently.
    scopes: Scopes,
}

/// What every module can see, by layer.
///
/// The layers are stored rather than flattened per module: the prelude is in
/// scope everywhere, and copying it into each module's map would cost the
/// prelude's size times the module count for no added answer.
#[derive(Debug, Default)]
struct Scopes {
    /// Each module's explicit `use` imports, by the local name — an alias, or a
    /// namespace import's `ns$member`.
    imports: IndexMap<ModuleSource, IndexMap<String, DefId>>,
    /// Each module's own declarations, including what its own `pub use`
    /// re-exports reach.
    own: IndexMap<ModuleSource, IndexMap<String, DefId>>,
    /// The prelude's public surface, then its implementation modules' own
    /// declarations. In scope in every module without a `use`, which is what
    /// makes `i32` and `List` universal and lets a sealed compiler item
    /// (`ReflectStruct`, `Member`) resolve for a module that never named it.
    prelude: IndexMap<String, DefId>,
}

impl Scopes {
    /// Which declaration `name` reaches from `module`, ignoring type-parameter
    /// binders: the module's imports, then its own declarations, then the
    /// prelude.
    ///
    /// The one implementation of the scope order.
    fn resolve(&self, module: &ModuleSource, name: &str) -> Option<DefId> {
        if let Some(def) = self.imports.get(module).and_then(|m| m.get(name)) {
            return Some(*def);
        }
        if let Some(def) = self.own.get(module).and_then(|m| m.get(name)) {
            return Some(*def);
        }
        self.prelude.get(name).copied()
    }

    fn build(
        modules: &IndexMap<ModuleSource, Module>,
        symbols: &SymbolTable,
        defs: &DefTable,
    ) -> Self {
        let mut out = Self::default();
        for (name, sym) in symbols.iter() {
            if is_prelude_module(sym.module_source())
                && let Some(def) = defs.of_ast_id(*name)
            {
                out.prelude.entry(sym.name.clone()).or_insert(def);
            }
        }
        // The prelude's own surface — its declarations and what it re-exports —
        // ranks above its implementation modules' internals.
        let prelude = ModuleSource::prelude();
        let mut surface: IndexMap<String, DefId> = IndexMap::default();
        for name in symbols.reexport_names(&prelude) {
            if let Some(sym) = symbols.lookup_in_module(&prelude, &name)
                && let Some(def) = defs.of_ast_id(sym.defined_at)
            {
                surface.insert(name, def);
            }
        }
        for (name, def) in out.prelude.drain(..) {
            surface.entry(name).or_insert(def);
        }
        out.prelude = surface;

        for module in modules.keys() {
            let imports: IndexMap<String, DefId> = symbols
                .imports_in(module)
                .filter_map(|(name, sym)| Some((name.to_string(), defs.of_ast_id(sym.defined_at)?)))
                .collect();
            let mut own: IndexMap<String, DefId> = symbols
                .get_module_symbols(module)
                .into_iter()
                .filter_map(|sym| Some((sym.name.clone(), defs.of_ast_id(sym.defined_at)?)))
                .collect();
            for name in symbols.reexport_names(module) {
                if let Some(sym) = symbols.lookup_in_module(module, &name)
                    && let Some(def) = defs.of_ast_id(sym.defined_at)
                {
                    own.entry(name).or_insert(def);
                }
            }
            out.imports.insert(module.clone(), imports);
            out.own.insert(module.clone(), own);
        }
        out
    }
}

impl Resolutions {
    /// Resolve every reference site in every loaded module, each from the
    /// module that wrote it.
    pub fn build(
        modules: &IndexMap<ModuleSource, Module>,
        symbols: &SymbolTable,
        defs: Arc<DefTable>,
    ) -> Self {
        let scopes = Scopes::build(modules, symbols, &defs);
        let mut refs = IndexMap::default();
        for (module_source, module) in modules {
            let mut resolver = Resolver {
                module: module_source,
                symbols,
                defs: &defs,
                binders: Vec::new(),
                scopes: &scopes,
                refs: &mut refs,
            };
            for item in &module.items {
                resolver.visit_item(item);
            }
        }
        Self { defs, refs, scopes }
    }

    /// Every declaration in the program.
    #[must_use]
    pub fn defs(&self) -> &Arc<DefTable> {
        &self.defs
    }

    /// The declaration `name` means from `module`'s vantage, with no reference
    /// site to key on.
    ///
    /// Runs the same scope order the walk runs at every site, minus the
    /// binders — a caller holding a bare name is outside any item's type
    /// parameters. One lookup, so a name-only answer and the answer that name's
    /// site gets cannot differ.
    #[must_use]
    pub fn declaration_named(&self, module: &ModuleSource, name: &str) -> Option<DefId> {
        self.scopes.resolve(module, name)
    }

    /// The `(module, name)` pair a declaration renders to.
    ///
    /// A rendering *out of* an identity, which is the only direction a name is
    /// allowed to travel — but it exists for the consumers whose keys are still
    /// spellings, and it goes when `DeclKey` does.
    #[must_use]
    pub fn decl_key(&self, def: DefId) -> (ModuleSource, String) {
        (
            self.defs.module(def).clone(),
            self.defs.name(def).to_string(),
        )
    }

    /// The declaration `module` itself declares under `name`.
    ///
    /// Not a scope lookup: `module` is the *declaring* module, so this asks a
    /// module about its own declarations rather than asking what a spelling
    /// means from some vantage. For the passes that still key on
    /// `(module, name)` pairs; it goes when they carry identities.
    #[must_use]
    pub fn declared_in(&self, module: &ModuleSource, name: &str) -> Option<DefId> {
        self.scopes
            .own
            .get(module)
            .and_then(|m| m.get(name))
            .copied()
    }

    /// The declaration a reference site names. `None` for a binder, a builtin
    /// shape, an unresolved name, or a site the walk never reached.
    #[must_use]
    pub fn declared(&self, site: AstId) -> Option<DefId> {
        match self.get(site)? {
            Resolution::Def(def) => Some(def),
            Resolution::Binder(_) | Resolution::Unresolved => None,
        }
    }

    /// The answer for a reference site, or `None` when the site was never
    /// walked — a coverage hole rather than an unresolved name.
    #[must_use]
    pub fn get(&self, site: AstId) -> Option<Resolution> {
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

fn is_prelude_module(module: &ModuleSource) -> bool {
    matches!(module, ModuleSource::Core { name } if name.as_str() == "prelude"
        || name.as_str().starts_with("prelude/"))
}

/// A module's declaration scope: the one implementation of "what does this name
/// mean here", and the only place a name becomes a [`DefId`].
struct Resolver<'a> {
    /// The vantage: the module every name in this walk is written in.
    module: &'a ModuleSource,
    symbols: &'a SymbolTable,
    defs: &'a DefTable,
    /// Binders in scope, innermost last. A name found here is the enclosing
    /// item's parameter and no module scope is consulted for it.
    binders: Vec<IndexMap<String, AstId>>,
    scopes: &'a Scopes,
    refs: &'a mut IndexMap<AstId, Resolution>,
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
    fn resolve_name(&self, name: &str) -> Resolution {
        if let Some(id) = self.binder(name) {
            return Resolution::Binder(id);
        }
        self.scopes
            .resolve(self.module, name)
            .map_or(Resolution::Unresolved, Resolution::Def)
    }

    fn record(&mut self, site: AstId, answer: Resolution) {
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
                self.record(assoc.id, Resolution::Unresolved);
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
                    Some(_) => Resolution::Unresolved,
                    None => self
                        .symbols
                        .imported(
                            self.module,
                            &crate::name::namespace_member_alias(&ns.namespace, &ns.name),
                        )
                        .and_then(|sym| self.defs.of_ast_id(sym.defined_at))
                        .map_or(Resolution::Unresolved, Resolution::Def),
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
