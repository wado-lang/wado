//! Declaration identity.
//!
//! Every declaration in the program gets a [`DefId`]: an opaque index into the
//! [`DefTable`] built once, after loading, from every module's items. It is the
//! identity the compiler compares, and this module mints no others —
//! [`DefTable::declare`] is the only constructor. Names travel the other way:
//! [`DefTable::name`] renders one for a diagnostic or for a mangle.
//!
//! The goal is that a spelling cannot reach an identity at all, so two modules'
//! same-named declarations cannot be confused for one. Every declaration index
//! is keyed by `DefId` and a trait head carries one, but the goal is not yet
//! reached: [`crate::tir::TypeTable::decl_named_in`] still answers
//! `(name, module)` with a declaration, first-declared-wins, and
//! [`crate::name::TypeHead::Declared`] is still a pair. Until a *type* head
//! carries a `DefId` too, what keeps two same-named type declarations apart is
//! that their rendered names differ — a convention each site maintains, not a
//! property of the key.
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

/// Identity of a declaration.
///
/// A dense index, minted only by [`DefTable::declare`]. Equality is declaration
/// identity: there is no spelling on it to compare instead, and no way to build
/// one for a declaration that does not exist.
///
/// [`crate::ast::AstId`] is deliberately not reused for this. It is the id type
/// of *every* node and [`AstId::fresh`] is public, so a use-site id would
/// type-check wherever a declaration id is expected and one could be minted from
/// nothing; it is also sparse, so per-declaration data could not be a `Vec`.
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
    /// Declared inside a function body rather than at module level.
    ///
    /// Two sibling functions may each declare a `struct Point`, and both are
    /// this module's `Point`, so the declared name alone cannot tell them
    /// apart. Every registry keyed by a rendered name — the WIR struct
    /// registry above all — needs the distinction, which is why
    /// [`crate::name::mangle_local_item_name`] exists; this is how a renderer
    /// knows to apply it.
    function_local: bool,
}

/// Every declaration in the program, by [`DefId`].
///
/// Built once from the symbol table, which already keys each declaration by its
/// declaring node. The table hands back what a declaration *is*; it deliberately
/// cannot answer what a *name* means — that question needs a module scope, and
/// only [`crate::resolve`] runs one.
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

    /// [`Self::build`], continuing an earlier table.
    ///
    /// A [`DefId`] is an index into one table, so a declaration fact that
    /// outlives the compile that produced it — the stdlib snapshot caches whole
    /// `ImplSig`s — is only readable if that compile's identities are still the
    /// same ones. Seeding keeps them: a declaration the seed already identifies
    /// keeps its `DefId`, and only what the seed never saw is minted here. This
    /// is what [`crate::tir::TypeTable`] does for `TypeId` and for the same
    /// reason.
    ///
    /// Both tables index the same declaration nodes because the stdlib AST is
    /// parsed once per process and shared, so an [`AstId`] means the same node
    /// in both.
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

    /// Mint a declaration for a unit test that needs one without parsing a
    /// module.
    ///
    /// Gated on `test` / the `test-util` feature, so production code still has
    /// no constructor — which is the property this design rests on. A test that
    /// wants a *struct type* rather than a declaration should use an anonymous
    /// shape instead.
    #[cfg(any(test, feature = "test-util"))]
    pub fn declare_for_test(&mut self, module: &ModuleSource, name: &str, kind: DefKind) -> DefId {
        self.declare(Def {
            ast_id: AstId::fresh(),
            module: module.clone(),
            name: name.to_string(),
            kind,
            visibility: Visibility::Public,
            span: None,
            parent: None,
            function_local: false,
            members: Vec::new(),
        })
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

/// The reachable functions that still turn a name into a declaration, each with
/// the reason it survives. Empty is the goal; see
/// `docs/wep-2026-08-12-declaration-identity.md`'s Remaining work.
///
/// A name reaching an identity is the whole shape this design removes: it is
/// what lets two same-named declarations answer for one another, and it is the
/// one property the type system cannot state. The test below is what keeps the
/// list from growing while it is being emptied.
#[cfg(test)]
const NAME_TO_IDENTITY: &[(&str, &str)] = &[
    (
        "declaration_named",
        "crate::resolve's own scope lookup — the one place a name is allowed \
         to become an identity. Its callers are what the WEP is emptying, not \
         this.",
    ),
    (
        "value_named",
        "the same lookup one tier longer, for a name written in value or \
         pattern position",
    ),
    (
        "declared_in",
        "asks a module about its own declarations rather than what a spelling \
         means from a vantage; goes when its callers carry identities",
    ),
    (
        "imported_as",
        "answers the import tier alone, for a caller to whom the *aliasing* is \
         the question — the one import fact that is not a scope lookup",
    ),
    (
        "decl_named_in",
        "TypeTable's first-declared-wins decl index; the largest remaining \
         group of callers, half of which name a stdlib type the compiler-item \
         registry should record instead",
    ),
    (
        "declare_for_test",
        "gated on `test` / the `test-util` feature, so production code has no \
         such constructor",
    ),
    (
        "canonical_assoc_const_key",
        "splits a use-site `Type::CONST` spelling, whose `Type` half has no \
         reference site of its own to ask; goes when the key does",
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

    /// Whether `signature` takes a module and a name and hands back a
    /// declaration.
    fn maps_a_name_to_an_identity(signature: &str) -> bool {
        let Some((params, ret)) = signature.split_once("->") else {
            return false;
        };
        let Some(params) = params.split_once('(').map(|(_, p)| p) else {
            return false;
        };
        ret.contains("DefId")
            && params.contains("ModuleSource")
            && (params.contains("&str") || params.contains("String"))
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
            for (offset, _) in source.match_indices("fn ") {
                let line_start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
                if !source[line_start..offset].trim_start().starts_with("pub") {
                    continue;
                }
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
            "these take a module and a name and hand back a declaration, so a \
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
}
