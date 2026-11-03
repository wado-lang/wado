//! A SCIP index of Wado source, the format rust-analyzer and other indexers
//! write (<https://github.com/scip-code/scip>, `scip.proto`).
//!
//! One analysis answers for the document it was run on: the declarations the
//! document makes, and the references its own code makes, wherever they lead.
//! An index of a package is one [`ScipDocument`] per file, so a reference made
//! in a file that nothing else imports is still found.
//!
//! A declaration's symbol is its module and its path of names, so the same
//! declaration gets the same symbol from every analysis that reaches it. A
//! binding inside a body (a parameter, a `let`, a type parameter) is a
//! document-local `local N`.

use indexmap::IndexMap;
use wado_compiler::ast::{
    self, AstId, AstVisitor, Expr, Function, Item, Pattern, walk_expr, walk_function, walk_item,
    walk_pattern,
};
use wado_compiler::module_source::ModuleSource;
use wado_compiler::token::Span;
use wado_compiler::unparse::unparse_type_into;

use crate::hover::find_hover_opts;
use crate::location::module_uri;
use crate::query::QueryContext;

/// `SymbolInformation.Kind` values from `scip.proto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Class = 7,
    Constant = 8,
    Enum = 11,
    EnumMember = 12,
    Field = 15,
    Function = 17,
    Interface = 21,
    Method = 26,
    Parameter = 37,
    Struct = 49,
    Trait = 53,
    Type = 54,
    TypeAlias = 55,
    TypeParameter = 58,
    Variable = 61,
}

/// `SymbolRole` bits from `scip.proto`.
const ROLE_DEFINITION: i32 = 0x1;
const ROLE_WRITE: i32 = 0x4;
const ROLE_READ: i32 = 0x8;

/// A range as `scip.proto`'s `Occurrence.range` packs it: 0-based
/// `[start_line, start_column, end_column]` on one line, or
/// `[start_line, start_column, end_line, end_column]`, in Unicode scalars.
pub type ScipRange = Vec<i32>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub range: ScipRange,
    pub symbol: String,
    pub roles: i32,
    /// The whole declaration a definition names, such as a function's body.
    pub enclosing_range: Option<ScipRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInformation {
    pub symbol: String,
    pub kind: SymbolKind,
    pub display_name: String,
    /// The declaration as hover renders it.
    pub signature: Option<String>,
    /// The symbol of the declaration this one is a member of; empty at the
    /// top level.
    pub enclosing_symbol: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScipDocument {
    pub relative_path: String,
    pub occurrences: Vec<Occurrence>,
    pub symbols: Vec<SymbolInformation>,
}

/// The document `ctx` analyzed. `root` is the directory URI paths are
/// relative to, with its trailing `/`.
pub(crate) fn document(ctx: &QueryContext, root: &str) -> ScipDocument {
    let entry = ctx.entry();
    let mut names = SymbolNames {
        ctx,
        root,
        modules: IndexMap::new(),
    };
    let own = names.declarations(entry);

    let mut occurrences = Vec::new();
    let mut symbols = Vec::new();
    for decl in own.values() {
        occurrences.push(Occurrence {
            range: scip_range(&decl.name_span),
            symbol: decl.symbol.clone(),
            roles: ROLE_DEFINITION,
            enclosing_range: (!decl.is_local()).then(|| scip_range(&decl.span)),
        });
        symbols.push(SymbolInformation {
            symbol: decl.symbol.clone(),
            kind: decl.kind,
            display_name: decl.name.clone(),
            signature: signature(ctx, &decl.name_span),
            enclosing_symbol: decl.enclosing.clone(),
        });
    }
    for (use_id, def_id) in ctx.sem.iter_references() {
        if ctx.sem.module_of_id(use_id) != Some(entry) {
            continue;
        }
        let (Some(span), Some(symbol)) = (ctx.sem.span_of_id(use_id), names.symbol_of(def_id))
        else {
            continue;
        };
        let roles = if ctx.sem.is_write_target(use_id) {
            ROLE_WRITE
        } else {
            ROLE_READ
        };
        occurrences.push(Occurrence {
            range: scip_range(&span),
            symbol,
            roles,
            enclosing_range: None,
        });
    }
    occurrences.sort_by(|a, b| a.range.cmp(&b.range).then_with(|| a.symbol.cmp(&b.symbol)));
    occurrences.dedup();
    ScipDocument {
        relative_path: names.module_name(entry),
        occurrences,
        symbols,
    }
}

/// The declaration at `name_span` as hover renders it, its code fence
/// stripped.
fn signature(ctx: &QueryContext, name_span: &Span) -> Option<String> {
    let line = u32::try_from(name_span.line - 1).expect("line fits in u32");
    let column = u32::try_from(name_span.column - 1).expect("column fits in u32");
    let hover = find_hover_opts(ctx, ctx.position_at(line, column), false)?;
    let text = hover.contents.value;
    let code = text
        .strip_prefix("```wado\n")
        .and_then(|rest| rest.split_once("\n```"))
        .map_or(text.as_str(), |(code, _)| code);
    Some(code.to_string())
}

fn scip_range(span: &Span) -> ScipRange {
    let n = |v: usize| i32::try_from(v - 1).expect("position fits in i32");
    if span.line == span.end_line {
        vec![n(span.line), n(span.column), n(span.end_column)]
    } else {
        vec![
            n(span.line),
            n(span.column),
            n(span.end_line),
            n(span.end_column),
        ]
    }
}

struct Decl {
    name: String,
    symbol: String,
    kind: SymbolKind,
    name_span: Span,
    /// The whole declaration.
    span: Span,
    /// The symbol of the declaration this one is a member of, or the body it
    /// is local to; empty at the top level.
    enclosing: String,
}

impl Decl {
    fn is_local(&self) -> bool {
        self.symbol.starts_with("local ")
    }
}

/// Every module's declarations by id, built on the first symbol asked of it.
struct SymbolNames<'a, 'c> {
    ctx: &'c QueryContext<'a>,
    root: &'c str,
    modules: IndexMap<ModuleSource, IndexMap<AstId, Decl>>,
}

impl SymbolNames<'_, '_> {
    fn declarations(&mut self, module: &ModuleSource) -> IndexMap<AstId, Decl> {
        let Some(ast) = self.ctx.sem.modules.get(module) else {
            return IndexMap::new();
        };
        let mut collector = DeclCollector {
            module: format!("wado . {} . ", escape_package(&self.module_name(module))),
            decls: IndexMap::new(),
            owners: Vec::new(),
            locals: 0,
        };
        for item in &ast.items {
            collector.visit_item(item);
        }
        collector.decls
    }

    fn symbol_of(&mut self, def: AstId) -> Option<String> {
        let module = self.ctx.sem.module_of_id(def)?.clone();
        if !self.modules.contains_key(&module) {
            let decls = self.declarations(&module);
            self.modules.insert(module.clone(), decls);
        }
        let decl = self.modules[&module].get(&def)?;
        // A local's number counts within its own document only.
        (module == *self.ctx.entry() || !decl.is_local()).then(|| decl.symbol.clone())
    }

    /// A file's path from the root, or a module's own name (`core:json`).
    fn module_name(&self, module: &ModuleSource) -> String {
        let uri = module_uri(self.ctx.entry(), module, self.ctx.uri);
        match uri.strip_prefix(self.root) {
            Some(relative) => relative.to_string(),
            None => uri,
        }
    }
}

/// A package name in a SCIP symbol, where a space separates fields.
fn escape_package(name: &str) -> String {
    name.replace(' ', "  ")
}

/// A name as one SCIP descriptor, backquoted where it is not a plain
/// identifier.
fn escape_name(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '+' | '-' | '$'))
    {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

fn type_text(ty: &ast::Type) -> String {
    let mut out = String::new();
    unparse_type_into(ty, &mut out);
    out
}

/// Names each declaration of one module as an AST walk reaches it.
struct DeclCollector {
    /// The symbol prefix: scheme and package.
    module: String,
    decls: IndexMap<AstId, Decl>,
    /// The declarations the walk is inside, innermost last, with the
    /// descriptor path a member's own descriptor follows.
    owners: Vec<String>,
    locals: u32,
}

impl DeclCollector {
    fn path(&self) -> &str {
        self.owners.last().map_or("", String::as_str)
    }

    /// Whether the walk is inside a body, where a binding is local.
    fn in_body(&self) -> bool {
        self.owners.iter().any(|path| path.ends_with(")."))
    }

    fn enclosing(&self) -> String {
        match self.path() {
            "" => String::new(),
            path => format!("{}{path}", self.module),
        }
    }

    fn global(
        &mut self,
        id: AstId,
        name: &str,
        descriptor: String,
        kind: SymbolKind,
        name_span: Span,
        span: Span,
    ) -> String {
        let path = format!("{}{descriptor}", self.path());
        self.decls.insert(
            id,
            Decl {
                name: name.to_string(),
                symbol: format!("{}{path}", self.module),
                kind,
                name_span,
                span,
                enclosing: self.enclosing(),
            },
        );
        path
    }

    /// A term: a field, a case, a global.
    fn term(&mut self, id: AstId, name: &str, kind: SymbolKind, name_span: Span, span: Span) {
        let descriptor = format!("{}.", escape_name(name));
        self.global(id, name, descriptor, kind, name_span, span);
    }

    fn local(&mut self, id: AstId, name: &str, kind: SymbolKind, name_span: Span) {
        let symbol = format!("local {}", self.locals);
        self.locals += 1;
        self.decls.insert(
            id,
            Decl {
                name: name.to_string(),
                symbol,
                kind,
                name_span,
                span: name_span,
                enclosing: self.enclosing(),
            },
        );
    }

    fn type_params(&mut self, params: &[ast::GenericParam]) {
        for tp in params {
            self.local(tp.id, &tp.name, SymbolKind::TypeParameter, tp.name_span);
        }
    }

    /// Declare a type and walk `item` with its members under it.
    fn type_item(
        &mut self,
        item: &Item,
        id: AstId,
        name: &str,
        kind: SymbolKind,
        name_span: Span,
        span: Span,
    ) {
        let path = self.global(
            id,
            name,
            format!("{}#", escape_name(name)),
            kind,
            name_span,
            span,
        );
        self.owners.push(path);
        match item {
            Item::Struct(s) => {
                self.type_params(&s.type_params);
                for f in &s.fields {
                    self.term(f.id, &f.name, SymbolKind::Field, f.name_span, f.span);
                }
            }
            Item::Enum(e) => {
                self.type_params(&e.type_params);
                for c in &e.cases {
                    self.term(c.id, &c.name, SymbolKind::EnumMember, c.name_span, c.span);
                }
            }
            Item::Variant(v) => {
                self.type_params(&v.type_params);
                for c in &v.cases {
                    self.term(c.id, &c.name, SymbolKind::EnumMember, c.name_span, c.span);
                }
            }
            Item::Flags(f) => {
                for flag in &f.flags {
                    let kind = SymbolKind::EnumMember;
                    self.term(flag.id, &flag.name, kind, flag.name_span, flag.span);
                }
            }
            _ => {}
        }
        walk_item(self, item);
        self.owners.pop();
    }
}

impl AstVisitor<'_> for DeclCollector {
    fn visit_item(&mut self, item: &Item) {
        match item {
            Item::Function(_) => walk_item(self, item),
            Item::Struct(s) => {
                self.type_item(item, s.id, &s.name, SymbolKind::Struct, s.name_span, s.span);
            }
            Item::Enum(e) => {
                self.type_item(item, e.id, &e.name, SymbolKind::Enum, e.name_span, e.span);
            }
            Item::Variant(v) => {
                self.type_item(item, v.id, &v.name, SymbolKind::Enum, v.name_span, v.span);
            }
            Item::Flags(f) => {
                self.type_item(item, f.id, &f.name, SymbolKind::Enum, f.name_span, f.span);
            }
            Item::Newtype(n) => self.type_item(
                item,
                n.id,
                &n.name,
                SymbolKind::TypeAlias,
                n.name_span,
                n.span,
            ),
            Item::Trait(t) => {
                self.type_item(item, t.id, &t.name, SymbolKind::Trait, t.name_span, t.span);
            }
            Item::Interface(i) => self.type_item(
                item,
                i.id,
                &i.name,
                SymbolKind::Interface,
                i.name_span,
                i.span,
            ),
            Item::BuiltinTypeDecl(d) => {
                self.type_item(item, d.id, &d.name, SymbolKind::Type, d.name_span, d.span);
            }
            Item::Resource(r) => {
                // A resource has no name span to declare; its methods still
                // take its name.
                self.owners.push(format!("{}#", escape_name(&r.name)));
                walk_item(self, item);
                self.owners.pop();
            }
            Item::Global(g) => {
                let kind = if g.mutable {
                    SymbolKind::Variable
                } else {
                    SymbolKind::Constant
                };
                self.term(g.id, &g.name, kind, g.name_span, g.span);
                walk_item(self, item);
            }
            Item::Impl(imp) => {
                let ty = type_text(&imp.ty);
                let path = match &imp.trait_type {
                    None => format!("{}#", escape_name(&ty)),
                    Some(tr) => format!(
                        "impl#[{}][{}]",
                        escape_name(&ty),
                        escape_name(&type_text(tr))
                    ),
                };
                self.owners.push(path);
                self.type_params(&imp.type_params);
                walk_item(self, item);
                self.owners.pop();
            }
            Item::Use(_)
            | Item::World(_)
            | Item::Test(_)
            | Item::TupleTypeDecl(_)
            | Item::Error(_) => {
                walk_item(self, item);
            }
        }
    }

    fn visit_function(&mut self, func: &Function) {
        let path = if self.in_body() {
            self.local(func.id, &func.name, SymbolKind::Function, func.name_span);
            None
        } else {
            let kind = if self.owners.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            Some(self.global(
                func.id,
                &func.name,
                format!("{}().", escape_name(&func.name)),
                kind,
                func.name_span,
                func.span,
            ))
        };
        let path = path.unwrap_or_else(|| format!("{}().", self.path()));
        self.owners.push(path);
        self.type_params(&func.type_params);
        for p in &func.params {
            self.local(p.id, &p.name, SymbolKind::Parameter, p.name_span);
        }
        walk_function(self, func);
        self.owners.pop();
    }

    fn visit_pattern(&mut self, pat: &Pattern) {
        if let Pattern::Ident { id, name, span } | Pattern::MutIdent { id, name, span } = pat {
            self.local(*id, name, SymbolKind::Variable, *span);
        }
        walk_pattern(self, pat);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Closure(c) = expr {
            for p in &c.params {
                self.local(p.id, &p.name, SymbolKind::Parameter, p.name_span);
            }
        }
        walk_expr(self, expr);
    }
}

/// `scip.proto`'s `Index` holding `documents`, as protobuf bytes.
#[must_use]
pub fn encode_index(project_root: &str, tool_version: &str, documents: &[ScipDocument]) -> Vec<u8> {
    let mut tool = Vec::new();
    put_string(&mut tool, 1, "wado");
    put_string(&mut tool, 2, tool_version);
    let mut metadata = Vec::new();
    put_message(&mut metadata, 2, &tool);
    put_string(&mut metadata, 3, project_root);
    // TextEncoding.UTF8
    put_varint_field(&mut metadata, 4, 1);

    let mut index = Vec::new();
    put_message(&mut index, 1, &metadata);
    for document in documents {
        put_message(&mut index, 2, &encode_document(document));
    }
    index
}

fn encode_document(document: &ScipDocument) -> Vec<u8> {
    let mut out = Vec::new();
    put_string(&mut out, 1, &document.relative_path);
    for o in &document.occurrences {
        let mut occurrence = Vec::new();
        put_packed(&mut occurrence, 1, &o.range);
        put_string(&mut occurrence, 2, &o.symbol);
        put_varint_field(&mut occurrence, 3, u64::from(o.roles.unsigned_abs()));
        if let Some(enclosing) = &o.enclosing_range {
            put_packed(&mut occurrence, 7, enclosing);
        }
        put_message(&mut out, 2, &occurrence);
    }
    for s in &document.symbols {
        let mut symbol = Vec::new();
        put_string(&mut symbol, 1, &s.symbol);
        put_varint_field(&mut symbol, 5, s.kind as u64);
        put_string(&mut symbol, 6, &s.display_name);
        if let Some(text) = &s.signature {
            let mut signature = Vec::new();
            put_string(&mut signature, 4, "wado");
            put_string(&mut signature, 5, text);
            put_message(&mut symbol, 7, &signature);
        }
        if !s.enclosing_symbol.is_empty() {
            put_string(&mut symbol, 8, &s.enclosing_symbol);
        }
        put_message(&mut out, 3, &symbol);
    }
    put_string(&mut out, 4, "wado");
    // PositionEncoding.UTF32CodeUnitOffsetFromLineStart: the compiler counts
    // columns in Unicode scalars.
    put_varint_field(&mut out, 6, 3);
    out
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(u8::try_from(value & 0x7f).expect("seven bits fit in u8") | 0x80);
        value >>= 7;
    }
    out.push(u8::try_from(value).expect("value is below 0x80"));
}

fn put_key(out: &mut Vec<u8>, field: u32, wire_type: u32) {
    put_varint(out, u64::from((field << 3) | wire_type));
}

fn put_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    put_key(out, field, 0);
    put_varint(out, value);
}

fn put_bytes(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_key(out, field, 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_string(out: &mut Vec<u8>, field: u32, text: &str) {
    put_bytes(out, field, text.as_bytes());
}

fn put_message(out: &mut Vec<u8>, field: u32, message: &[u8]) {
    put_bytes(out, field, message);
}

/// A packed `repeated int32`; every value here is a position, so none is
/// negative.
fn put_packed(out: &mut Vec<u8>, field: u32, values: &[i32]) {
    let mut packed = Vec::new();
    for &v in values {
        put_varint(&mut packed, u64::from(v.unsigned_abs()));
    }
    put_bytes(out, field, &packed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_ctx::with_ctx;
    use crate::text::PositionEncoding;

    async fn doc(source: &str) -> ScipDocument {
        with_ctx(source, PositionEncoding::Utf32, |ctx| {
            document(ctx, "file:///")
        })
        .await
    }

    fn symbol<'d>(d: &'d ScipDocument, name: &str) -> &'d SymbolInformation {
        d.symbols
            .iter()
            .find(|s| s.display_name == name)
            .unwrap_or_else(|| panic!("no symbol {name} in {:#?}", d.symbols))
    }

    /// The occurrences of `name`'s symbol, as `(line, roles)`.
    fn uses(d: &ScipDocument, name: &str) -> Vec<(i32, i32)> {
        let target = &symbol(d, name).symbol;
        d.occurrences
            .iter()
            .filter(|o| &o.symbol == target)
            .map(|o| (o.range[0], o.roles))
            .collect()
    }

    const SOURCE: &str = "struct P { x: i32 }
impl P {
    fn get(&self) -> i32 { return self.x; }
}
fn f(p: P) -> i32 {
    let mut n = p.get();
    n = n + p.x;
    return n;
}
";

    #[test]
    fn names_declarations_by_module_and_path() {
        futures::executor::block_on(async {
            let d = doc(SOURCE).await;
            assert_eq!(d.relative_path, "test.wado");
            assert_eq!(symbol(&d, "P").symbol, "wado . test.wado . P#");
            assert_eq!(symbol(&d, "x").symbol, "wado . test.wado . P#x.");
            assert_eq!(symbol(&d, "get").symbol, "wado . test.wado . P#get().");
            assert_eq!(symbol(&d, "f").symbol, "wado . test.wado . f().");
            assert_eq!(symbol(&d, "get").kind, SymbolKind::Method);
            assert_eq!(symbol(&d, "x").enclosing_symbol, "wado . test.wado . P#");
            assert!(symbol(&d, "n").symbol.starts_with("local "));
        });
    }

    #[test]
    fn records_definitions_and_references() {
        futures::executor::block_on(async {
            let d = doc(SOURCE).await;
            assert_eq!(uses(&d, "get"), vec![(2, ROLE_DEFINITION), (5, ROLE_READ)]);
            assert_eq!(
                uses(&d, "x"),
                vec![(0, ROLE_DEFINITION), (2, ROLE_READ), (6, ROLE_READ)]
            );
            assert_eq!(
                uses(&d, "n"),
                vec![
                    (5, ROLE_DEFINITION),
                    (6, ROLE_WRITE),
                    (6, ROLE_READ),
                    (7, ROLE_READ)
                ]
            );
        });
    }

    #[test]
    fn a_definition_carries_the_whole_declaration() {
        futures::executor::block_on(async {
            let d = doc(SOURCE).await;
            let get = &symbol(&d, "get").symbol;
            let def = d
                .occurrences
                .iter()
                .find(|o| &o.symbol == get && o.roles == ROLE_DEFINITION)
                .expect("definition of get");
            assert_eq!(def.range, vec![2, 7, 10]);
            assert_eq!(def.enclosing_range, Some(vec![2, 4, 43]));
        });
    }

    #[test]
    fn signatures_come_from_hover() {
        futures::executor::block_on(async {
            let d = doc(SOURCE).await;
            assert_eq!(
                symbol(&d, "f").signature.as_deref(),
                Some("fn f(p: P) -> i32")
            );
        });
    }

    #[test]
    fn encodes_as_protobuf() {
        let d = ScipDocument {
            relative_path: "a.wado".to_string(),
            occurrences: vec![Occurrence {
                range: vec![0, 3, 4],
                symbol: "s".to_string(),
                roles: ROLE_DEFINITION,
                enclosing_range: None,
            }],
            symbols: Vec::new(),
        };
        let bytes = encode_index("file:///r/", "0", &[d]);
        // Index.documents (field 2) holds Document.relative_path (field 1).
        let document = [0x0a, 6, b'a', b'.', b'w', b'a', b'd', b'o'];
        assert!(bytes.windows(document.len()).any(|w| w == document));
        // Occurrence.range, packed: field 1, length 3, then 0, 3, 4.
        let range = [0x0a, 3, 0, 3, 4];
        assert!(bytes.windows(range.len()).any(|w| w == range));
    }
}
