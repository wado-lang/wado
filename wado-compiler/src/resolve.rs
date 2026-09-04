//! Reference resolution: one answer per reference site, recorded under the
//! site's own [`AstId`] because which declaration a spelling means is a fact
//! about the module that wrote it. The only place a name becomes a [`DefId`].
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
    /// The cases the types above bring with them — `Some`, `Ok`, an `enum`
    /// case written bare. Their own tier, under every type tier, because a type
    /// always shadows a same-named case: an imported `FieldKind::List` must not
    /// hide the prelude's `List`.
    cases: IndexMap<ModuleSource, IndexMap<String, DefId>>,
    prelude_cases: IndexMap<String, DefId>,
}

impl Scopes {
    /// Which declaration `name` reaches from `module` in type position,
    /// ignoring type-parameter binders: the module's imports, then its own
    /// declarations, then the prelude.
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

    /// [`Self::resolve`], then the case tier.
    ///
    /// A name in value or pattern position can mean a case, which no type
    /// position can; the two orders share every tier but this last one, so they
    /// share the implementation rather than layering a second scope beside it.
    fn resolve_value(&self, module: &ModuleSource, name: &str) -> Option<DefId> {
        self.resolve(module, name)
            .or_else(|| self.cases.get(module).and_then(|m| m.get(name)).copied())
            .or_else(|| self.prelude_cases.get(name).copied())
    }

    /// The cases the declarations in `from` bring into scope with them.
    ///
    /// A namespace import contributes none. `use ns from "m"` enters `m`'s
    /// declarations under `ns$Name` aliases so that `ns::Name` resolves, and
    /// those are reachable *only* through that qualification — letting their
    /// cases into this tier would answer a bare `Name` with a case the module
    /// can only name as `ns::Name`.
    fn collect_cases(defs: &DefTable, from: &IndexMap<String, DefId>) -> IndexMap<String, DefId> {
        let mut out: IndexMap<String, DefId> = IndexMap::default();
        for (name, def) in from {
            if name.contains(crate::name::NAMESPACE_MEMBER_SEP) {
                continue;
            }
            if !matches!(
                defs.kind(*def),
                crate::defs::DefKind::Variant
                    | crate::defs::DefKind::Enum
                    | crate::defs::DefKind::Flags
            ) {
                continue;
            }
            for member in defs.members(*def) {
                out.entry(defs.name(*member).to_string()).or_insert(*member);
            }
        }
        out
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
        out.prelude_cases = Self::collect_cases(defs, &out.prelude);

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
            // Both tiers bring their cases, imports ranking first for the same
            // reason the type tiers do.
            let mut cases = Self::collect_cases(defs, &imports);
            for (name, def) in Self::collect_cases(defs, &own) {
                cases.entry(name).or_insert(def);
            }
            out.imports.insert(module.clone(), imports);
            out.own.insert(module.clone(), own);
            out.cases.insert(module.clone(), cases);
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
                locals: Vec::new(),
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

    /// Every declaration `module` explicitly `use`d, by the local name it
    /// wrote — an alias where it wrote one, and the `ns$member` name a
    /// namespace import registers.
    pub fn imports_in(&self, module: &ModuleSource) -> impl Iterator<Item = (&str, DefId)> {
        self.scopes
            .imports
            .get(module)
            .into_iter()
            .flatten()
            .map(|(name, def)| (name.as_str(), *def))
    }

    /// The declaration the prelude puts in scope under `name`.
    ///
    /// The prelude tier alone. It takes no vantage and cannot be given one: the
    /// prelude is in scope in every module, including one carrying
    /// `#![no_prelude]`, so there is no module from which this answers
    /// differently and no import that can steer it.
    #[must_use]
    pub fn prelude_decl(&self, name: &str) -> Option<DefId> {
        self.scopes.prelude.get(name).copied()
    }

    /// The declaration `module` explicitly `use`d under the local name `name`.
    ///
    /// The import tier alone, so an alias answers with what it aliases and a
    /// name the module never imported answers with nothing — the question a
    /// caller asks when the *aliasing* is the point, not what the name means.
    #[must_use]
    pub fn imported_as(&self, module: &ModuleSource, name: &str) -> Option<DefId> {
        self.scopes
            .imports
            .get(module)
            .and_then(|m| m.get(name))
            .copied()
    }

    /// Every declaration `module` may name — what each name it can write
    /// reaches, through the one scope order: its `use` imports, then its own
    /// (including what its `pub use` re-exports reach), then the prelude's.
    ///
    /// A nearer tier shadows a farther one, so a module declaring its own
    /// `Add` reaches that one and not the prelude's. Enumerating the tiers
    /// instead would put both in scope at once, which no name ever resolves to.
    #[must_use]
    pub fn decls_in_scope(&self, module: &ModuleSource) -> crate::hashmap::IndexSet<DefId> {
        let names = |t: &IndexMap<ModuleSource, IndexMap<String, DefId>>| {
            t.get(module)
                .map(|m| m.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        names(&self.scopes.imports)
            .into_iter()
            .chain(names(&self.scopes.own))
            .chain(self.scopes.prelude.keys().cloned())
            .filter_map(|name| self.scopes.resolve(module, &name))
            .collect()
    }

    /// The declaration a reference site names. `None` for a binder, a builtin
    /// shape, an unresolved name, or a site the walk never reached.
    #[must_use]
    pub fn declared(&self, site: AstId) -> Option<DefId> {
        match self.get(site) {
            Resolution::Def(def) => Some(def),
            Resolution::Binder(_) | Resolution::Unresolved => None,
        }
    }

    /// [`Self::declared`] for a consumer that also walks synthesised AST, whose
    /// nodes carry ids no module wrote and no walk answered for.
    #[must_use]
    pub fn declared_if_walked(&self, site: AstId) -> Option<DefId> {
        match self.walked(site)? {
            Resolution::Def(def) => Some(def),
            Resolution::Binder(_) | Resolution::Unresolved => None,
        }
    }

    /// The whole answer for a site the walk reached, `None` for a node it
    /// never saw.
    ///
    /// The three cases stay apart for a caller that must tell "this names a
    /// binder" from "this names nothing" from "no walk saw this node" — the
    /// last being the only one for which any other source of truth is honest.
    #[must_use]
    pub fn walked(&self, site: AstId) -> Option<Resolution> {
        self.refs.get(&site).copied()
    }

    /// The answer for a reference site, or `None` when the site was never
    /// walked — a coverage hole rather than an unresolved name.
    #[must_use]
    /// Total: every reference site has an answer, because the walk reaches
    /// every one and a name that reaches nothing is [`Resolution::Unresolved`]
    /// rather than a missing entry. A caller therefore has no "no answer" case
    /// to write a fallback for — which is the point, since that fallback is
    /// where a spelling used to be re-resolved.
    pub fn get(&self, site: AstId) -> Resolution {
        self.refs.get(&site).copied().unwrap_or_else(|| {
            panic!("every reference site is resolved before elaboration, {site:?} was not")
        })
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
    /// Items declared inside the function body being walked, innermost block
    /// last. Filled as the walk passes each declaration, because a local item
    /// is visible only after it — like a `let`, and unlike a module-level
    /// declaration.
    locals: Vec<IndexMap<String, DefId>>,
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

    /// The declaration a name written in this module refers to. The layers are
    /// ordered, and the order is the rule: the enclosing item's binders, the
    /// module's explicit imports keyed by local name, its own declarations, then
    /// the prelude and its implementation modules. Own declarations outranking
    /// the prelude is what makes a local `trait Left` mean itself (#1298).
    fn resolve_value_name(&self, name: &str) -> Resolution {
        if let Some(id) = self.binder(name) {
            return Resolution::Binder(id);
        }
        if let Some(def) = self
            .locals
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
        {
            return Resolution::Def(def);
        }
        self.scopes
            .resolve_value(self.module, name)
            .map_or(Resolution::Unresolved, Resolution::Def)
    }

    fn resolve_name(&self, name: &str) -> Resolution {
        if let Some(id) = self.binder(name) {
            return Resolution::Binder(id);
        }
        if let Some(def) = self
            .locals
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
        {
            return Resolution::Def(def);
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
            Item::Trait(t) => {
                // Inside a trait, `Self` is bounded by that trait. The
                // elaborator mints that bound rather than the programmer, and a
                // minted spelling would be a reference the walk never saw — so
                // the declaration node answers for itself and the bound names
                // it instead of respelling its name.
                if let Some(def) = self.defs.of_ast_id(t.id) {
                    self.record(t.id, Resolution::Def(def));
                }
                (&t.type_params, Some(t.id))
            }
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

    /// A block's local items are in scope for the whole of it, wherever they
    /// are written, and for none of it once it closes.
    fn visit_block(&mut self, block: &ast::Block) {
        let mut scope = IndexMap::default();
        for stmt in &block.stmts {
            // A local `impl` block writes no name for the scope to hold.
            if let ast::Stmt::Item(item) = stmt
                && !matches!(**item, ast::Item::Impl(_))
                && let Some(def) = self.defs.of_ast_id(item.id())
            {
                scope.insert(self.defs.name(def).to_string(), def);
            }
        }
        self.locals.push(scope);
        ast::walk_block(self, block);
        self.locals.pop();
    }

    /// A struct pattern's qualifier names a type, so it resolves like one —
    /// through the `ns$Type` alias when it arrived through a namespace import,
    /// the same spelling a struct *literal*'s name uses.
    fn visit_pattern(&mut self, pat: &ast::Pattern) {
        if let ast::Pattern::Struct {
            type_name: Some(name),
            type_name_id: Some(id),
            ..
        } = pat
        {
            let answer = self.resolve_name(&name.replace("::", "$"));
            self.record(*id, answer);
        }
        ast::walk_pattern(self, pat);
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

    /// A qualified path names declarations with the segments before its last:
    /// the head, which `Trait::method` names, and the one just before the final
    /// name, which `Type::CONST` qualifies its constant with. They coincide for
    /// a two-segment path.
    fn visit_expr(&mut self, expr: &ast::Expr) {
        if let ast::Expr::StructLiteral(lit) = expr
            && let (Some(name), Some(name_id)) = (lit.name.as_deref(), lit.name_id)
        {
            let answer = self.resolve_name(&name.replace("::", "$"));
            self.record(name_id, answer);
        }
        // `ns::member` resolves under the `ns$member` alias; a path that is no
        // such import answers `Unresolved` and its segments below name it.
        if let ast::Expr::Ident(ident) = expr {
            let written = ident.name.replace("::", "$");
            let answer = self.resolve_value_name(&written);
            self.record(ident.id, answer);
        }
        if let ast::Expr::Ident(ident) = expr
            && let [head, rest @ ..] = ident.segments.as_slice()
            && !rest.is_empty()
        {
            let answer = self.resolve_name(&head.name);
            self.record(head.id, answer);
            let owner = ident.segments.len() - 2;
            if owner > 0 {
                let qualified = format!(
                    "{}${}",
                    ident.segments[owner - 1].name,
                    ident.segments[owner].name
                );
                let answer = self.resolve_name(&qualified);
                self.record(ident.segments[owner].id, answer);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::{InMemoryCompilerHost, LogLevel};
    use crate::module_source::ModuleSourceInterner;

    /// Resolve two modules together and hand back the table, so a test can ask
    /// what a name means from either vantage.
    fn resolve(entry: &str, other: &str) -> (Resolutions, ModuleSource, ModuleSource) {
        let (r, e, o, _) = resolve_with_ast(entry, other);
        (r, e, o)
    }

    fn resolve_with_ast(
        entry: &str,
        other: &str,
    ) -> (
        Resolutions,
        ModuleSource,
        ModuleSource,
        IndexMap<ModuleSource, Module>,
    ) {
        // One interner: `ModuleSource` equality is pointer identity, so two
        // interners would mint values that never compare equal and every import
        // would resolve to a module the map does not hold.
        let interner = std::rc::Rc::new(std::cell::RefCell::new(ModuleSourceInterner::new()));
        let entry_source = interner.borrow_mut().local("./main.wado");
        let other_source = interner.borrow_mut().local("./other.wado");
        let mut modules: IndexMap<ModuleSource, Module> = IndexMap::default();
        for (source, text) in [(&entry_source, entry), (&other_source, other)] {
            let lexed = crate::lexer::lex(text);
            assert!(lexed.errors.is_empty(), "lex error: {:?}", lexed.errors);
            let ast = crate::parser::Parser::new(lexed.tokens)
                .parse_strict()
                .expect("parse error");
            modules.insert(source.clone(), ast);
        }
        let host = InMemoryCompilerHost::new();
        let logger = crate::logger::Logger::new(&host, LogLevel::Error);
        let mut analyzer = crate::analyze::Analyzer::new(&logger).with_interner(interner);
        let _ = analyzer.analyze_loaded_modules(
            &modules,
            &entry_source,
            crate::hashmap::IndexSet::default(),
        );
        assert!(
            host.diagnostics().is_empty(),
            "analyze reported: {:?}",
            host.diagnostics()
        );
        let symbols = analyzer.into_symbols();
        let defs = Arc::new(DefTable::build(&modules, &symbols));
        (
            Resolutions::build(&modules, &symbols, defs),
            entry_source,
            other_source,
            modules,
        )
    }

    /// Every shape a reference can take, so a walk that stops short of one is
    /// visible here rather than as a consumer falling back to a spelling.
    fn reference_sites(ty: &Type, out: &mut Vec<AstId>) {
        match ty {
            Type::Named(n) => out.push(n.id),
            Type::Generic(g) => {
                out.push(g.id);
                for a in &g.args {
                    reference_sites(a, out);
                }
            }
            Type::NamespacedGeneric(ns) => {
                out.push(ns.id);
                for a in &ns.args {
                    reference_sites(a, out);
                }
            }
            Type::Reference(inner) | Type::MutReference(inner) => reference_sites(inner, out),
            Type::Tuple(elems) => {
                for e in elems {
                    reference_sites(e, out);
                }
            }
            Type::Function(f) => {
                for pty in &f.params {
                    reference_sites(pty, out);
                }
                reference_sites(&f.return_type, out);
            }
            Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => {}
        }
    }

    /// The whole class this design exists to end: one spelling, two modules,
    /// two declarations. Each vantage answers with its own.
    #[test]
    fn one_spelling_in_two_modules_is_two_declarations() {
        let (r, entry, other) = resolve(
            "pub struct Widget { a: i32 }",
            "pub struct Widget { b: i32 }",
        );
        let here = r.scopes.resolve(&entry, "Widget").unwrap();
        let there = r.scopes.resolve(&other, "Widget").unwrap();
        assert_ne!(here, there);
        assert_eq!(r.defs().module(here), &entry);
        assert_eq!(r.defs().module(there), &other);
    }

    /// A case is reachable unqualified wherever its type is, in value position
    /// only — and a type of the same name shadows it, whichever tier each is on.
    #[test]
    fn a_type_shadows_a_same_named_case() {
        let (r, entry, other) = resolve(
            r#"use { FieldKind } from "./other.wado";
               pub struct List { n: i32 }"#,
            "pub variant FieldKind { List(i32), Leaf }",
        );
        // `Leaf` reaches nothing in type position and its case in value position.
        assert!(r.scopes.resolve(&entry, "Leaf").is_none());
        let leaf = r.scopes.resolve_value(&entry, "Leaf").unwrap();
        assert_eq!(r.defs().kind(leaf), crate::defs::DefKind::VariantCase);
        assert_eq!(r.defs().module(leaf), &other);

        // `List` is both a case of the imported variant and this module's own
        // struct. The type wins in both positions.
        let list = r.scopes.resolve_value(&entry, "List").unwrap();
        assert_eq!(list, r.scopes.resolve(&entry, "List").unwrap());
        assert_eq!(r.defs().kind(list), crate::defs::DefKind::Struct);
        assert_eq!(r.defs().module(list), &entry);
    }

    /// The import tier answers under the name the module wrote, and holds no
    /// cases — those ride a tier only value position consults.
    #[test]
    fn imports_in_answers_under_the_local_name_and_holds_no_cases() {
        let (r, entry, other) = resolve(
            r#"use { FieldKind as FK } from "./other.wado";"#,
            "pub variant FieldKind { List(i32), Leaf }",
        );
        let imports: Vec<(&str, DefId)> = r.imports_in(&entry).collect();
        assert_eq!(imports.len(), 1, "{imports:?}");
        let (local_name, def) = imports[0];
        assert_eq!(local_name, "FK");
        assert_eq!(r.defs().name(def), "FieldKind");
        assert_eq!(r.defs().module(def), &other);
        // The variant's cases reach value position without entering this tier.
        assert!(r.scopes.resolve_value(&entry, "Leaf").is_some());
    }

    /// A namespace import enters its members under the qualification the
    /// module can name them by.
    #[test]
    fn imports_in_records_a_namespace_member_qualified() {
        let (r, entry, _) = resolve(
            r#"use ns from "./other.wado";"#,
            "pub struct Widget { a: i32 }",
        );
        let names: Vec<&str> = r.imports_in(&entry).map(|(name, _)| name).collect();
        assert!(names.contains(&"ns$Widget"), "{names:?}");
    }

    /// A `struct` declared in a function body shadows a module-level one of
    /// the same name for the rest of that body, and nowhere else.
    #[test]
    fn a_function_local_item_shadows_only_inside_its_body() {
        let source = r#"
            pub struct Widget { a: i32 }
            pub fn local() -> i32 {
                struct Widget { b: i32 }
                let w: Widget = Widget { b: 1 };
                return w.b;
            }
            pub fn outer(w: Widget) -> i32 { return w.a; }
        "#;
        let (r, entry, _, modules) = resolve_with_ast(source, "");
        let defs = r.defs();
        // The module-level `Widget` is the one `outer`'s parameter names — the
        // walk answers for that site from outside the shadowing body.
        let module_level = modules[&entry]
            .items
            .iter()
            .find_map(|item| match item {
                Item::Struct(d) if d.name == "Widget" => defs.of_ast_id(d.id),
                _ => None,
            })
            .unwrap();

        let mut sites = Vec::new();
        for item in &modules[&entry].items {
            let Item::Function(f) = item else { continue };
            let named = |ty: &Type| head_site(ty).map(|s| (f.name.clone(), r.get(s)));
            sites.extend(f.params.iter().filter_map(|p| named(&p.ty)));
            if let Some(body) = &f.body {
                for stmt in &body.stmts {
                    if let ast::Stmt::Let(l) = stmt
                        && let Some(ty) = &l.ty
                    {
                        sites.extend(named(ty));
                    }
                }
            }
        }
        let answer = |func: &str| sites.iter().find(|(n, _)| n == func).map(|(_, a)| *a);

        let Some(Resolution::Def(inner)) = answer("local") else {
            panic!(
                "the local `Widget` annotation resolved to {:?}",
                answer("local")
            );
        };
        assert_ne!(inner, module_level);
        assert_eq!(defs.name(inner), "Widget");
        assert_eq!(answer("outer"), Some(Resolution::Def(module_level)));
    }

    #[test]
    fn an_alias_resolves_to_the_declaration_it_aliases() {
        let (r, entry, other) = resolve(
            r#"use { Widget as W } from "./other.wado";"#,
            "pub struct Widget { b: i32 }",
        );
        let aliased = r.scopes.resolve(&entry, "W").unwrap();
        assert_eq!(r.defs().module(aliased), &other);
        assert_eq!(r.defs().name(aliased), "Widget");
        // The alias is the only spelling in scope; the original is not.
        assert!(r.scopes.resolve(&entry, "Widget").is_none());
    }

    /// The table answers only for the sites the walk records, so its coverage
    /// is part of the design: a site the walk misses reads as "no answer", and
    /// every consumer of that site falls back to what it can derive from a
    /// spelling. Each shape a reference can take is exercised here.
    #[test]
    fn the_walk_reaches_every_reference_site() {
        let source = r#"
            pub trait Greet { fn hello(&self) -> i32; }
            pub trait Louder: Greet { fn shout(&self) -> i32; }
            pub struct Widget { a: i32 }
            pub struct Pair<A, B> { a: A, b: B }

            pub struct Holder<T: Greet> {
                plain: i32,
                generic: Pair<i32, T>,
                by_ref: &Widget,
                nested: Pair<Widget, Pair<i32, Widget>>,
                tuple: [Widget, i32],
                func: fn(Widget) -> i32,
            }

            pub fn takes<U: Louder>(u: U) -> Widget {
                return Widget { a: 1 };
            }

            impl Greet for Widget {
                fn hello(&self) -> i32 { return 1; }
            }
        "#;
        let (r, entry, _, modules) = resolve_with_ast(source, "pub struct Other { b: i32 }");
        let module = modules.get(&entry).expect("the entry module was walked");

        let mut sites: Vec<AstId> = Vec::new();
        let mut bounds: Vec<AstId> = Vec::new();
        let walk_params = |params: &[GenericParam], bounds: &mut Vec<AstId>| {
            for p in params {
                bounds.extend(p.bounds.iter().map(|b| b.id));
            }
        };
        for item in &module.items {
            match item {
                Item::Struct(s) => {
                    walk_params(&s.type_params, &mut bounds);
                    for f in &s.fields {
                        reference_sites(&f.ty, &mut sites);
                    }
                }
                Item::Trait(t) => bounds.extend(t.supertraits.iter().map(|b| b.id)),
                Item::Function(f) => {
                    walk_params(&f.type_params, &mut bounds);
                    for prm in &f.params {
                        reference_sites(&prm.ty, &mut sites);
                    }
                    if let Some(ret) = &f.return_type {
                        reference_sites(ret, &mut sites);
                    }
                }
                Item::Impl(i) => {
                    reference_sites(&i.ty, &mut sites);
                    if let Some(tr) = &i.trait_type {
                        reference_sites(tr, &mut sites);
                    }
                }
                _ => {}
            }
        }
        assert!(
            sites.len() > 12 && bounds.len() >= 3,
            "expected a rich site set, got {} types and {} bounds",
            sites.len(),
            bounds.len()
        );
        for site in sites.into_iter().chain(bounds) {
            // `get` is total, so a site the walk missed panics here naming
            // itself — the assertion is the call.
            let _ = r.get(site);
        }
    }

    /// A name reaching no declaration is its own answer, never a key made up
    /// from the writing module.
    #[test]
    fn a_name_reaching_nothing_is_unresolved() {
        let (r, entry, _) = resolve(
            "pub struct Widget { a: i32 }",
            "pub struct Other { b: i32 }",
        );
        assert!(r.scopes.resolve(&entry, "Absent").is_none());
    }

    /// A reference site answers from the module that wrote it, so the same
    /// spelling written in two modules resolves to two declarations.
    #[test]
    fn a_reference_site_answers_from_the_module_that_wrote_it() {
        let (r, entry, other) = resolve(
            "pub struct Widget { a: i32 }\npub fn here(w: Widget) {}",
            "pub struct Widget { b: i32 }\npub fn there(w: Widget) {}",
        );
        let mut seen: Vec<(ModuleSource, DefId)> = Vec::new();
        for (site, answer) in &r.refs {
            if let Resolution::Def(def) = answer
                && r.defs().name(*def) == "Widget"
            {
                seen.push((r.defs().module(*def).clone(), *def));
                let _ = site;
            }
        }
        assert!(seen.iter().any(|(m, _)| m == &entry));
        assert!(seen.iter().any(|(m, _)| m == &other));
        let first = seen[0].1;
        assert!(
            seen.iter().any(|(_, d)| *d != first),
            "the two modules' `Widget` references must not share one identity"
        );
    }
}
