//! Declaration identity: every declaration gets a [`DefId`], the identity the
//! compiler compares. [`DefTable::declare`] is its only constructor, and names
//! travel the other way through [`DefTable::name`].
//!
//! See `docs/wep-2026-08-12-declaration-identity.md`.

use crate::ast::{AstId, Item, Module, Visibility};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::symbol::{SymbolKind, SymbolTable};
use crate::token::Span;

/// One member of a declaration, as [`DefTable::declare_members`] collects it.
struct Member {
    ast_id: AstId,
    name: String,
    kind: DefKind,
    /// The member's own visibility where it declares one — a struct field does,
    /// a case does not and takes its type's.
    visibility: Option<Visibility>,
    span: Span,
}

/// Collect one item's members, so each arm of [`DefTable::declare_members`]
/// states only what differs.
fn members<'a>(
    kind: DefKind,
    items: impl Iterator<Item = (AstId, &'a String, Option<Visibility>, Span)>,
) -> Vec<Member> {
    items
        .map(|(ast_id, name, visibility, span)| Member {
            ast_id,
            name: name.clone(),
            kind,
            visibility,
            span,
        })
        .collect()
}

/// Identity of a declaration: a dense index minted only by [`DefTable::declare`],
/// so equality is declaration identity and no id exists for a declaration that
/// does not.
///
/// Not [`crate::ast::AstId`]: that is every node's id type and `fresh` is public,
/// so a use-site id would type-check here; it is also sparse.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefId(u32);

impl std::fmt::Debug for DefId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Def({})", self.0)
    }
}

/// What a declaration declares.
///
/// The question "is this name a trait / an effect / a resource" is asked of the
/// declaration, not scanned for by bare name across the per-kind indexes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DefKind {
    Function,
    Effect,
    Struct,
    Enum,
    Flags,
    Variant,
    Trait,
    Newtype,
    /// A named, definition-less builtin type (`pub type Array<T>;`).
    BuiltinType,
    Resource,
    World,
    Global,
    /// A local binding. Declared by the elaborator for navigation, not by the
    /// collect pass over module items.
    Variable,
    /// A struct field.
    Field,
    /// An `enum` case.
    EnumCase,
    /// A `variant` case.
    VariantCase,
    /// A `flags` member.
    FlagsMember,
    /// A method declared by a trait, a resource, or an effect interface.
    Method,
}

impl DefKind {
    fn of(kind: &SymbolKind) -> Self {
        match kind {
            SymbolKind::Function(_) => Self::Function,
            SymbolKind::Effect(_) => Self::Effect,
            SymbolKind::Struct(_) => Self::Struct,
            SymbolKind::Enum(_) => Self::Enum,
            SymbolKind::Flags(_) => Self::Flags,
            SymbolKind::Variant(_) => Self::Variant,
            SymbolKind::Trait(_) => Self::Trait,
            SymbolKind::Newtype(_) => Self::Newtype,
            SymbolKind::BuiltinType => Self::BuiltinType,
            SymbolKind::Variable(_) => Self::Variable,
            SymbolKind::Resource(_) => Self::Resource,
            SymbolKind::World(_) => Self::World,
            SymbolKind::Global(_) => Self::Global,
        }
    }

    /// Whether a declaration of this kind names a type.
    #[must_use]
    pub fn is_type(self) -> bool {
        matches!(
            self,
            Self::Struct
                | Self::Enum
                | Self::Flags
                | Self::Variant
                | Self::Newtype
                | Self::BuiltinType
                | Self::Resource
        )
    }

    /// Whether a declaration of this kind is a case of a sum or bitmask type.
    ///
    /// A case is reachable unqualified wherever its type is in scope, and a
    /// type of the same name always shadows it.
    #[must_use]
    pub fn is_case(self) -> bool {
        matches!(self, Self::EnumCase | Self::VariantCase | Self::FlagsMember)
    }
}

/// The facts every declaration carries, whatever it declares.
#[derive(Debug, Clone)]
struct Def {
    ast_id: AstId,
    module: ModuleSource,
    name: String,
    kind: DefKind,
    visibility: Visibility,
    span: Option<Span>,
    /// The declaration this one is a member of — the struct a field belongs to,
    /// the variant a case belongs to, the trait a method belongs to. `None` for
    /// a module-level declaration.
    parent: Option<DefId>,
    /// Members declared inside this one, in source order.
    members: Vec<DefId>,
    /// Declared inside a function body rather than at module level. Two sibling
    /// functions may each declare a `struct Point`, and the mangled namespaces
    /// downstream are name-keyed, so a renderer applies
    /// [`crate::name::mangle_local_item_name`] to keep the two spellings apart.
    /// This tells it to. Nothing reads that spelling back — which declaration a
    /// name means is this table's question, never a rendering's.
    function_local: bool,
}

/// Every declaration in the program, by [`DefId`]. Hands back what a declaration
/// *is*; what a *name* means needs a module scope, which only
/// [`crate::resolve`] runs.
#[derive(Debug, Default, Clone)]
pub struct DefTable {
    defs: Vec<Def>,
    by_ast_id: IndexMap<AstId, DefId>,
}

impl DefTable {
    /// Identify every declaration in the program: the module-level ones the
    /// symbol table collected, then the members each of them declares.
    #[must_use]
    pub fn build(modules: &IndexMap<ModuleSource, Module>, symbols: &SymbolTable) -> Self {
        Self::build_seeded(None, modules, symbols)
    }

    /// [`Self::build`], continuing an earlier table so a cached declaration fact
    /// stays readable: a declaration the seed identifies keeps its [`DefId`], and
    /// only what the seed never saw is minted here.
    ///
    /// The stdlib AST is parsed once per process and shared, so an [`AstId`] means
    /// the same node in both tables.
    #[must_use]
    pub fn build_seeded(
        seed: Option<&Self>,
        modules: &IndexMap<ModuleSource, Module>,
        symbols: &SymbolTable,
    ) -> Self {
        let mut table = seed.cloned().unwrap_or_default();
        for (ast_id, symbol) in symbols.iter() {
            if table.by_ast_id.contains_key(ast_id) {
                continue;
            }
            table.declare(Def {
                ast_id: *ast_id,
                module: symbol.module_source().clone(),
                name: symbol.name.clone(),
                kind: DefKind::of(&symbol.kind),
                visibility: symbol.visibility,
                span: symbol.span,
                parent: None,
                function_local: false,
                members: Vec::new(),
            });
        }
        for (module, ast) in modules {
            // The tuple family declares a type the symbol table skips — it has
            // no written name, only the `[]` every mangler spells it with — so
            // it would otherwise be the one type in the language with no
            // declaration to name it.
            for item in &ast.items {
                if let Item::TupleTypeDecl(decl) = item
                    && !table.by_ast_id.contains_key(&decl.id)
                {
                    table.declare(Def {
                        ast_id: decl.id,
                        module: module.clone(),
                        name: crate::name::TUPLE_TYPE_NAME.to_string(),
                        kind: DefKind::BuiltinType,
                        visibility: decl.visibility,
                        span: Some(decl.span),
                        parent: None,
                        function_local: false,
                        members: Vec::new(),
                    });
                }
            }
            table.declare_members(module, ast);
            let mut locals = LocalItems {
                table: &mut table,
                module,
            };
            for item in &ast.items {
                crate::ast::walk_item(&mut locals, item);
            }
        }
        table
    }

    /// Mint an identity. Private on purpose: it is the only constructor of a
    /// [`DefId`], and [`Self::build`] is its only caller, so no pass downstream
    /// can invent a declaration.
    fn declare(&mut self, def: Def) -> DefId {
        let id = DefId(u32::try_from(self.defs.len()).expect("DefId space exhausted"));
        self.by_ast_id.insert(def.ast_id, id);
        self.defs.push(def);
        id
    }

    /// Identify each member a module's items declare, under its owner.
    ///
    /// A member the symbol table already collected — an effect or resource
    /// method, registered there under its `Owner::method` name so it can be
    /// imported — keeps that identity and is only linked to its owner here.
    /// Nothing gets two.
    fn declare_members(&mut self, module: &ModuleSource, ast: &Module) {
        for item in &ast.items {
            self.declare_item_members(module, item);
        }
    }

    /// Identify a function-local item (`Stmt::Item`) and its members.
    ///
    /// A local `struct` is a declaration like any other — two functions writing
    /// the same spelling declare two of them — so it gets an identity rather
    /// than a mangled storage name standing in for one.
    fn declare_local_item(&mut self, module: &ModuleSource, item: &Item) {
        let (kind, name) = match item {
            Item::Struct(d) => (DefKind::Struct, &d.name),
            Item::Enum(d) => (DefKind::Enum, &d.name),
            Item::Variant(d) => (DefKind::Variant, &d.name),
            Item::Flags(d) => (DefKind::Flags, &d.name),
            Item::Newtype(d) => (DefKind::Newtype, &d.name),
            Item::Trait(d) => (DefKind::Trait, &d.name),
            _ => return,
        };
        if self.by_ast_id.contains_key(&item.id()) {
            return;
        }
        self.declare(Def {
            ast_id: item.id(),
            module: module.clone(),
            name: name.clone(),
            kind,
            visibility: item.visibility().unwrap_or(Visibility::Private),
            span: Some(item.span()),
            parent: None,
            function_local: true,
            members: Vec::new(),
        });
        self.declare_item_members(module, item);
    }

    fn declare_item_members(&mut self, module: &ModuleSource, item: &Item) {
        let (owner, members) = match item {
            Item::Struct(s) => (
                s.id,
                members(
                    DefKind::Field,
                    s.fields
                        .iter()
                        .map(|f| (f.id, &f.name, Some(f.visibility), f.span)),
                ),
            ),
            Item::Enum(e) => (
                e.id,
                members(
                    DefKind::EnumCase,
                    e.cases.iter().map(|c| (c.id, &c.name, None, c.span)),
                ),
            ),
            Item::Variant(v) => (
                v.id,
                members(
                    DefKind::VariantCase,
                    v.cases.iter().map(|c| (c.id, &c.name, None, c.span)),
                ),
            ),
            Item::Flags(f) => (
                f.id,
                members(
                    DefKind::FlagsMember,
                    f.flags.iter().map(|m| (m.id, &m.name, None, m.span)),
                ),
            ),
            Item::Trait(t) => (
                t.id,
                members(
                    DefKind::Method,
                    t.methods
                        .iter()
                        .map(|m| (m.id, &m.name, Some(m.visibility), m.span)),
                ),
            ),
            Item::Resource(r) => (
                r.id,
                members(
                    DefKind::Method,
                    r.methods.iter().map(|m| (m.id, &m.name, None, m.span)),
                ),
            ),
            Item::Interface(i) => (
                i.id,
                members(
                    DefKind::Method,
                    i.methods.iter().map(|m| (m.id, &m.name, None, m.span)),
                ),
            ),
            _ => return,
        };
        let Some(owner) = self.of_ast_id(owner) else {
            return;
        };
        let owner_visibility = self.visibility(owner);
        for member in members {
            // A seed already linked this member to this owner; linking again
            // would list it twice under the owner.
            if self.of_ast_id(member.ast_id).map(|id| self.get(id).parent) == Some(Some(owner)) {
                continue;
            }
            let id = self.of_ast_id(member.ast_id).unwrap_or_else(|| {
                self.declare(Def {
                    ast_id: member.ast_id,
                    module: module.clone(),
                    name: member.name,
                    kind: member.kind,
                    visibility: member.visibility.unwrap_or(owner_visibility),
                    span: Some(member.span),
                    parent: Some(owner),
                    function_local: false,
                    members: Vec::new(),
                })
            });
            self.defs[id.0 as usize].parent = Some(owner);
            self.defs[owner.0 as usize].members.push(id);
        }
    }

    fn get(&self, def: DefId) -> &Def {
        &self.defs[def.0 as usize]
    }

    /// The module that declares `def`.
    #[must_use]
    pub fn module(&self, def: DefId) -> &ModuleSource {
        &self.get(def).module
    }

    /// The name `def`'s declaration writes — a rendering for diagnostics and for
    /// mangling, never a key.
    #[must_use]
    pub fn name(&self, def: DefId) -> &str {
        &self.get(def).name
    }

    /// Whether `def` was declared inside a function body.
    ///
    /// A renderer needs this: two sibling functions' `struct Point` are two
    /// declarations with one name, and a name-keyed registry downstream cannot
    /// tell them apart unless the rendering does.
    #[must_use]
    pub fn is_function_local(&self, def: DefId) -> bool {
        self.defs[def.0 as usize].function_local
    }

    /// The node that declares `def`.
    #[must_use]
    pub fn ast_id(&self, def: DefId) -> AstId {
        self.get(def).ast_id
    }

    #[must_use]
    pub fn kind(&self, def: DefId) -> DefKind {
        self.get(def).kind
    }

    #[must_use]
    pub fn visibility(&self, def: DefId) -> Visibility {
        self.get(def).visibility
    }

    #[must_use]
    pub fn span(&self, def: DefId) -> Option<Span> {
        self.get(def).span
    }

    /// The declaration `def` is a member of, or `None` for a module-level one.
    #[must_use]
    pub fn parent(&self, def: DefId) -> Option<DefId> {
        self.get(def).parent
    }

    /// The members `def` declares, in source order.
    #[must_use]
    pub fn members(&self, def: DefId) -> &[DefId] {
        &self.get(def).members
    }

    /// The declaration a node declares, for the passes that hold a declaring
    /// node rather than an identity — the symbol table, the compiler-item
    /// registry, an LSP navigation edge. `None` for a node that declares
    /// nothing.
    ///
    /// This is not a name lookup: the node is already the declaration.
    #[must_use]
    pub fn of_ast_id(&self, id: AstId) -> Option<DefId> {
        self.by_ast_id.get(&id).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Every declaration, in collect order.
    pub fn iter(&self) -> impl Iterator<Item = DefId> + '_ {
        (0..self.defs.len()).map(|i| DefId(i as u32))
    }
}

/// Reaches the items declared inside function bodies, which the symbol table —
/// which collects module-level declarations only — never saw.
struct LocalItems<'a> {
    table: &'a mut DefTable,
    module: &'a ModuleSource,
}

impl crate::ast::AstVisitor for LocalItems<'_> {
    fn visit_stmt(&mut self, stmt: &crate::ast::Stmt) {
        if let crate::ast::Stmt::Item(item) = stmt {
            self.table.declare_local_item(self.module, item);
        }
        crate::ast::walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_source::ModuleSourceInterner;
    use crate::symbol::{StructSymbol, TraitSymbol};

    fn table() -> (DefTable, SymbolTable, ModuleSource, ModuleSource) {
        let mut symbols = SymbolTable::new();
        let mut interner = ModuleSourceInterner::new();
        let a = interner.local("./a.wado");
        let b = interner.local("./b.wado");
        symbols.define(
            &a,
            AstId::fresh(),
            "Widget",
            SymbolKind::Struct(StructSymbol { fields: vec![] }),
            Visibility::Public,
            None,
        );
        symbols.define(
            &b,
            AstId::fresh(),
            "Widget",
            SymbolKind::Struct(StructSymbol { fields: vec![] }),
            Visibility::Public,
            None,
        );
        symbols.define(
            &a,
            AstId::fresh(),
            "Greet",
            SymbolKind::Trait(TraitSymbol {
                methods: vec![],
                type_params: vec![],
            }),
            Visibility::Public,
            None,
        );
        let defs = DefTable::build(&IndexMap::default(), &symbols);
        (defs, symbols, a, b)
    }

    #[test]
    fn two_modules_same_name_are_two_identities() {
        let (defs, symbols, a, b) = table();
        let widget_a = defs
            .of_ast_id(symbols.lookup_in_module(&a, "Widget").unwrap().defined_at)
            .unwrap();
        let widget_b = defs
            .of_ast_id(symbols.lookup_in_module(&b, "Widget").unwrap().defined_at)
            .unwrap();
        assert_ne!(widget_a, widget_b);
        assert_eq!(defs.name(widget_a), defs.name(widget_b));
        assert_eq!(defs.module(widget_a), &a);
        assert_eq!(defs.module(widget_b), &b);
    }

    #[test]
    fn kind_answers_what_a_declaration_declares() {
        let (defs, symbols, a, _) = table();
        let greet = defs
            .of_ast_id(symbols.lookup_in_module(&a, "Greet").unwrap().defined_at)
            .unwrap();
        let widget = defs
            .of_ast_id(symbols.lookup_in_module(&a, "Widget").unwrap().defined_at)
            .unwrap();
        assert_eq!(defs.kind(greet), DefKind::Trait);
        assert!(!defs.kind(greet).is_type());
        assert!(defs.kind(widget).is_type());
    }

    #[test]
    fn a_node_that_declares_nothing_has_no_identity() {
        let (defs, _, _, _) = table();
        assert!(defs.of_ast_id(AstId::fresh()).is_none());
        assert_eq!(defs.len(), 3);
    }

    /// A member is a declaration too, so a case and a field get identities of
    /// their own rather than being reached by name off their owner.
    #[test]
    fn members_are_declarations_under_their_owner() {
        let source = r#"
            pub struct Point { x: i32, y: i32 }
            pub variant Shape { Circle(i32), Square(i32) }
            pub trait Greet { fn hello(&self) -> i32; }
        "#;
        let (defs, module) = build_from_source(source);
        let owner = |name: &str| {
            defs.iter()
                .find(|d| defs.name(*d) == name && defs.parent(*d).is_none())
                .unwrap()
        };
        let member_names =
            |d: DefId| -> Vec<&str> { defs.members(d).iter().map(|m| defs.name(*m)).collect() };

        let point = owner("Point");
        assert_eq!(member_names(point), ["x", "y"]);
        assert_eq!(defs.kind(defs.members(point)[0]), DefKind::Field);

        let shape = owner("Shape");
        assert_eq!(member_names(shape), ["Circle", "Square"]);
        assert!(defs.kind(defs.members(shape)[0]).is_case());
        assert_eq!(defs.parent(defs.members(shape)[0]), Some(shape));

        let greet = owner("Greet");
        assert_eq!(member_names(greet), ["hello"]);
        assert_eq!(defs.kind(defs.members(greet)[0]), DefKind::Method);
        assert_eq!(defs.module(defs.members(greet)[0]), &module);
    }

    /// A method the symbol table already collected (registered as
    /// `Owner::method` so it can be imported) keeps that one identity and is
    /// only linked to its owner.
    #[test]
    fn a_member_the_symbol_table_collected_gets_no_second_identity() {
        let source = r#"
            pub interface Logger { fn log(msg: String); }
        "#;
        let (defs, _) = build_from_source(source);
        let logger = defs
            .iter()
            .find(|d| defs.kind(*d) == DefKind::Effect)
            .unwrap();
        assert_eq!(defs.members(logger).len(), 1);
        let method = defs.members(logger)[0];
        // Registered under its importable name, and reached only once.
        assert_eq!(defs.name(method), "Logger::log");
        assert_eq!(
            defs.iter()
                .filter(|d| defs.ast_id(*d) == defs.ast_id(method))
                .count(),
            1
        );
    }

    /// A `struct` written inside a function body is a declaration, so two
    /// functions writing the same spelling declare two of them.
    #[test]
    fn a_function_local_item_is_a_declaration_of_its_own() {
        let source = r#"
            pub fn here() -> i32 {
                struct Widget { a: i32 }
                return 1;
            }
            pub fn there() -> i32 {
                struct Widget { b: i32 }
                return 2;
            }
        "#;
        let (defs, _) = build_from_source(source);
        let widgets: Vec<DefId> = defs.iter().filter(|d| defs.name(*d) == "Widget").collect();
        assert_eq!(widgets.len(), 2);
        assert_ne!(widgets[0], widgets[1]);
        assert_eq!(defs.kind(widgets[0]), DefKind::Struct);
        let field_names =
            |d: DefId| -> Vec<&str> { defs.members(d).iter().map(|m| defs.name(*m)).collect() };
        assert_eq!(field_names(widgets[0]), ["a"]);
        assert_eq!(field_names(widgets[1]), ["b"]);
    }

    /// A cached declaration fact carries a [`DefId`], so a later compile that
    /// re-identifies the same declarations must hand back the same ones — the
    /// stdlib snapshot reads its `ImplSig`s back this way.
    #[test]
    fn seeding_keeps_a_seen_declaration_at_its_identity() {
        let source = r#"
            pub struct Point { x: i32, y: i32 }
            pub trait Greet { fn hello(&self) -> i32; }
        "#;
        let (seed, modules, symbols, _) = analyze_source(source);
        let again = DefTable::build_seeded(Some(&seed), &modules, &symbols);

        assert_eq!(again.len(), seed.len());
        for def in seed.iter() {
            assert_eq!(again.of_ast_id(seed.ast_id(def)), Some(def));
            assert_eq!(again.name(def), seed.name(def));
            assert_eq!(again.members(def), seed.members(def));
        }
    }

    fn build_from_source(source: &str) -> (DefTable, ModuleSource) {
        let (defs, _, _, module) = analyze_source(source);
        (defs, module)
    }

    fn analyze_source(
        source: &str,
    ) -> (
        DefTable,
        IndexMap<ModuleSource, crate::ast::Module>,
        SymbolTable,
        ModuleSource,
    ) {
        use crate::compiler_host::{InMemoryCompilerHost, LogLevel};
        let lexed = crate::lexer::lex(source);
        assert!(lexed.errors.is_empty(), "lex error: {:?}", lexed.errors);
        let ast = crate::parser::Parser::new(lexed.tokens)
            .parse_strict()
            .expect("parse error");
        let mut interner = ModuleSourceInterner::new();
        let module = interner.local("./main.wado");
        let mut modules: IndexMap<ModuleSource, crate::ast::Module> = IndexMap::default();
        modules.insert(module.clone(), ast);

        let host = InMemoryCompilerHost::new();
        let logger = crate::logger::Logger::new(&host, LogLevel::Error);
        let mut analyzer = crate::analyze::Analyzer::new(&logger);
        let _ =
            analyzer.analyze_loaded_modules(&modules, &module, crate::hashmap::IndexSet::default());
        let symbols = analyzer.into_symbols();
        let defs = DefTable::build(&modules, &symbols);
        (defs, modules, symbols, module)
    }
}

/// Every reachable function that still derives a declaration from a name
/// without a reference site, each with the reason it survives. This is the
/// whole list, not a sample: the scan below fails on a new one and equally on a
/// stale entry.
///
/// A declaration is whatever *identifies* one, so the list spans both
/// currencies: [`DefId`], and the [`crate::symbol::Symbol`] rows that carry the
/// declaring node. See `docs/wep-2026-08-12-declaration-identity.md`.
#[cfg(test)]
const NAME_TO_IDENTITY: &[(&str, &str)] = &[
    // The one scope.
    (
        "resolve",
        "`Scopes::resolve` is the one scope, so it is the one place this shape \
         belongs. Every other entry here exists because it is *not* this.",
    ),
    (
        "resolve_value",
        "`Scopes::resolve` in the value namespace; see above.",
    ),
    // The recorded facts the frame derivation is built from. Each is one tier,
    // none is a scope, and none takes a vantage a caller could get wrong.
    (
        "imported_as",
        "answers the import tier alone, for a caller to whom the *aliasing* is \
         the question — the one import fact that is not a scope lookup",
    ),
    (
        "prelude_decl",
        "the prelude tier alone. It cannot be given a vantage: the prelude is in \
         scope in every module, so there is none from which it answers \
         differently.",
    ),
    (
        "decls_named",
        "hands back *every* declaration written under the name and picks none, \
         so it is not an answer. It holds what modules declare, never what they \
         import, and takes no module — the caller filters by a frame of its own.",
    ),
    // The frame derivation itself: the three tiers above, in order, over a frame
    // that is the walk's own position rather than a caller's argument.
    (
        "decl_key_or_local",
        "the frame derivation, for a caller holding a rendered head whose \
         reference site is not at hand. A caller *with* a site reaches \
         `decl_key_at`, which asks the site.",
    ),
    (
        "declaration",
        "`TypeLookup`'s copy of the frame derivation, for the same callers one \
         layer down. `declaration_at` is the sited entry point.",
    ),
    (
        "namespace_member",
        "`imported_as` on the `ns$Name` alias a namespace import registers — \
         the import tier answering the qualification the programmer wrote.",
    ),
    (
        "scoped_trait_decl_key",
        "`declaration` filtered to the trait index, for the `TypeSystem` \
         queries that hold a scope and a bound's spelling rather than its site.",
    ),
    (
        "bound_declaring_assoc_type",
        "asks which of a *binder's* bounds declares an associated-type name. \
         The binder is the walk's own, not a module's, and the trait comes from \
         each bound's reference site.",
    ),
    // A rendering compared against a declaration's own, because the index that
    // produced it stores a spelling.
    (
        "impl_target_decl_key",
        "walks a receiver type's newtype chain for the link whose rendering \
         equals the head an impl was found under. The name is the impl index's, \
         not a caller's; it goes when that index carries `DefId`s.",
    ),
    // The same derivation in the `Symbol` currency. A symbol row carries the
    // declaring node, so it identifies a declaration as surely as a `DefId`
    // does; §2 deletes this half by moving what these answer onto `DefTable`.
    (
        "symbol_named",
        "the frame derivation, handing back the symbol row instead of the \
         identity. `symbol_at` is the sited entry point, and answers from the \
         same table so annotate and reify cannot disagree.",
    ),
    (
        "imported",
        "`imported_as` in the symbol currency: the module's own import list, \
         no prelude fallback and no declaration of its own, so a caller orders \
         those layers itself.",
    ),
    (
        "lookup_in_module",
        "what a module declares under a name, re-export chains followed. The \
         derivation's second tier in the symbol currency; it reads one module's \
         own declarations and cannot reach another's imports.",
    ),
    (
        "lookup_in_module_with_visited",
        "`lookup_in_module` carrying its own re-export cycle guard.",
    ),
    // The Component Model boundary.
    (
        "cm_decl_in",
        "permanent. A WIT name has no Wado reference site, and \
         `CmInterfaceRegistry` parses its own copy of the WASI modules, so no \
         declaring node it could record is in this program's `DefTable`. \
         `wado-from-idl` declares each WIT name once per generated module, so \
         the pair identifies one declaration by construction.",
    ),
    (
        "cm_decl",
        "`cm_decl_in` from the synthesis side, which resolves the interface's \
         module first; see above.",
    ),
];

#[cfg(test)]
mod enforcement {
    /// The signature starting at `start`, up to the `{` or `;` that ends it.
    fn signature_at(source: &str, start: usize) -> Option<&str> {
        let mut depth = 0i32;
        for (i, byte) in source[start..].bytes().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'{' | b';' if depth == 0 => return Some(&source[start..start + i]),
                _ => {}
            }
        }
        None
    }

    /// The parameter types of `params`, split on the commas between them —
    /// not on the ones inside `IndexMap<(ModuleSource, AstId), _>`, whose
    /// spelling would otherwise read as a module parameter beside a name one.
    fn param_types(params: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut start = 0usize;
        for (i, c) in params.char_indices() {
            match c {
                '<' | '(' | '[' => depth += 1,
                '>' | ')' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    out.push(&params[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        out.push(&params[start..]);
        out.into_iter()
            .filter_map(|p| p.split_once(':').map(|(_, ty)| ty.trim()))
            .collect()
    }

    /// Whether `ty` mentions the type `name` — as a whole word, so
    /// `SymbolNotation` and `SymbolResolveError` are not `Symbol`.
    fn mentions_type(ty: &str, name: &str) -> bool {
        let boundary = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        ty.match_indices(name).any(|(i, _)| {
            boundary(ty[..i].chars().next_back()) && boundary(ty[i + name.len()..].chars().next())
        })
    }

    /// Whether `signature` takes a name and no reference site and hands back a
    /// declaration.
    ///
    /// A declaration is whatever identifies one, so both currencies count: a
    /// `DefId`, and a `Symbol` row, which carries the declaring node and so
    /// answers the same question one table earlier.
    ///
    /// A `ModuleSource` parameter is deliberately *not* part of the shape. Every
    /// by-name declaration lookup in the elaborator is a method whose vantage is
    /// `&self`, so requiring the module as an argument exempted the whole class
    /// this design is about and left the scan reading four functions that merely
    /// happen to pass it.
    ///
    /// An `AstId` parameter *is* an exemption, and the only one: a function
    /// handed the reference site reads the answer the resolve pass recorded for
    /// it. Deriving one is the shape; asking for one is not.
    fn maps_a_name_to_an_identity(signature: &str) -> bool {
        let Some((params, ret)) = signature.split_once("->") else {
            return false;
        };
        let Some(params) = params.split_once('(').map(|(_, p)| p) else {
            return false;
        };
        let types = param_types(params.trim_end().trim_end_matches(')'));
        let is_name = |ty: &&str| matches!(*ty, "&str" | "String" | "&String");
        let is_site = |ty: &&str| ty.contains("AstId");
        let hands_back_a_declaration = mentions_type(ret, "DefId") || mentions_type(ret, "Symbol");
        hands_back_a_declaration && types.iter().any(is_name) && !types.iter().any(is_site)
    }

    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("wado-compiler/src is readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// A consumer that can turn a spelling into a declaration can compare two
    /// declarations by spelling, which is the whole class of bug this design
    /// removes (WEP 2026-08-12). The type system cannot state the absence of
    /// such a function, so this does.
    #[test]
    fn no_reachable_function_turns_a_name_into_an_identity() {
        let mut files = Vec::new();
        rust_sources(std::path::Path::new("src"), &mut files);
        assert!(!files.is_empty(), "found no sources to scan");

        let mut offenders: Vec<String> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for file in &files {
            let source = std::fs::read_to_string(file).expect("a readable source file");
            // A test module's helpers are not reachable API.
            let source = source
                .split_once("\n#[cfg(test)]\nmod tests {")
                .map_or(source.as_str(), |(before, _)| before);
            // Every function, whatever its visibility. A private helper of this
            // shape is reachable by everything in its module and hands out the
            // same wrong answer, so exempting it would let the class grow
            // wherever the module is large.
            for (offset, _) in source.match_indices("fn ") {
                let Some(signature) = signature_at(source, offset) else {
                    continue;
                };
                if !maps_a_name_to_an_identity(signature) {
                    continue;
                }
                let name = signature["fn ".len()..]
                    .split(['(', '<'])
                    .next()
                    .unwrap_or_default()
                    .trim();
                match super::NAME_TO_IDENTITY.iter().find(|(n, _)| *n == name) {
                    Some((allowed, _)) => seen.push(allowed),
                    None => offenders.push(format!("{}::{name}", file.display())),
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these turn a name into a declaration with no reference site, so a \
             consumer holding only a spelling can obtain an identity and \
             compare it: {offenders:?}. Give the caller the reference site \
             instead — or, if it truly cannot have one, register it in \
             NAME_TO_IDENTITY with the reason."
        );
        let stale: Vec<&str> = super::NAME_TO_IDENTITY
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| !seen.contains(n))
            .collect();
        assert!(
            stale.is_empty(),
            "NAME_TO_IDENTITY lists functions that no longer exist: {stale:?}. \
             Delete the entries — the list is what is left to remove."
        );
    }

    /// A field of this shape is the same defect a function of it is: a map from
    /// a spelling to a declaration answers for whatever key a caller can build,
    /// so it turns a rendering back into an identity. The scan above reads
    /// signatures, so it cannot see one.
    ///
    /// One survives, and it is the Component Model boundary: `TypeTable`'s
    /// `decl_index` is what `cm_decl_in` reads, for a WIT name that has no Wado
    /// reference site. The other was `local_item_renders`, which read a
    /// function-local item's `{name}@{AstId}` rendering back into its
    /// declaration; it is gone, and with it the last place this design traded a
    /// mechanism for a convention.
    #[test]
    fn no_map_turns_a_name_into_an_identity() {
        const ALLOWED: &[&str] = &["decl_index"];

        let mut files = Vec::new();
        rust_sources(std::path::Path::new("src"), &mut files);
        let mut offenders: Vec<String> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for file in &files {
            let source = std::fs::read_to_string(file).expect("a readable source file");
            for (offset, _) in source.match_indices("IndexMap<(") {
                let Some(entry) = source[..offset].rfind('\n') else {
                    continue;
                };
                let decl = &source[entry + 1..offset];
                let Some(field) = decl.rsplit_once(':').map(|(before, _)| before.trim()) else {
                    continue;
                };
                let field = field
                    .rsplit(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("");
                // `IndexMap<(K1, K2), V>` — the key pair, then the value.
                let rest = &source[offset + "IndexMap<(".len()..];
                let Some(close) = rest.find(')') else {
                    continue;
                };
                let (key, value) = rest.split_at(close);
                let Some(value) = value.split_once(',').map(|(_, v)| v) else {
                    continue;
                };
                let value = value.split('>').next().unwrap_or("");
                let keyed_by_a_name = key.contains("String") && key.contains("ModuleSource");
                let holds_a_declaration =
                    mentions_type(value, "DefId") || mentions_type(value, "AstId");
                if !(keyed_by_a_name && holds_a_declaration) {
                    continue;
                }
                match ALLOWED.iter().find(|allowed| **allowed == field) {
                    Some(allowed) => seen.push(allowed),
                    None => offenders.push(format!("{}::{field}", file.display())),
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these map a spelling and a module to a declaration, so a consumer \
             holding only a rendering can read an identity back out of it: \
             {offenders:?}. Key the map by the declaration instead."
        );
        let stale: Vec<&str> = ALLOWED
            .iter()
            .copied()
            .filter(|n| !seen.contains(n))
            .collect();
        assert!(
            stale.is_empty(),
            "no such map any more: {stale:?}. Delete the entry — the list is \
             what is left to remove."
        );
    }
}
