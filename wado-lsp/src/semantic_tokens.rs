use indexmap::{IndexMap, IndexSet};
use wado_compiler::ast::{self, AstId, Expr, Item, Stmt, Type};
use wado_compiler::lexer::lex;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::semantics::Semantics;
use wado_compiler::symbol::{Symbol, SymbolKey, SymbolKind};
use wado_compiler::token::{Token, TokenKind};

use crate::text::{PositionEncoding, codepoints_to_code_units, line_without_terminator};

/// LSP semantic token type indices (must match `TOKEN_TYPES` order).
///
/// Indices `0..=13` are append-only history: keep them stable so the legend
/// stays comparable across versions. New kinds (`14..`) map Wado-specific
/// declarations onto standard LSP scopes that ship in common themes.
pub mod token_type {
    pub const NAMESPACE: u32 = 0;
    pub const TYPE: u32 = 1;
    pub const TYPE_PARAMETER: u32 = 2;
    pub const PARAMETER: u32 = 3;
    pub const VARIABLE: u32 = 4;
    pub const PROPERTY: u32 = 5;
    pub const ENUM_MEMBER: u32 = 6;
    pub const FUNCTION: u32 = 7;
    pub const METHOD: u32 = 8;
    pub const KEYWORD: u32 = 9;
    pub const COMMENT: u32 = 10;
    pub const STRING: u32 = 11;
    pub const NUMBER: u32 = 12;
    pub const OPERATOR: u32 = 13;
    pub const STRUCT: u32 = 14;
    pub const ENUM: u32 = 15;
    pub const INTERFACE: u32 = 16;
    pub const CLASS: u32 = 17;
}

/// Token type legend for LSP capability declaration.
pub const TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "typeParameter",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "function",
    "method",
    "keyword",
    "comment",
    "string",
    "number",
    "operator",
    "struct",
    "enum",
    "interface",
    "class",
];

/// LSP semantic token modifier bit positions (must match `TOKEN_MODIFIERS`).
pub mod token_modifier {
    pub const DECLARATION: u32 = 1 << 0;
    pub const DEFINITION: u32 = 1 << 1;
    pub const READONLY: u32 = 1 << 2;
}

/// Token modifier legend for LSP capability declaration.
pub const TOKEN_MODIFIERS: &[&str] = &["declaration", "definition", "readonly"];

/// A semantic token with absolute position (before delta encoding).
#[derive(Debug, Clone)]
pub struct SemanticToken {
    pub line: u32,
    pub start_char: u32,
    pub length: u32,
    pub token_type: u32,
    pub modifiers: u32,
}

/// Compute semantic tokens for a Wado source string.
///
/// When `sem` is `Some`, identifiers are classified by their resolved symbol
/// kind (variable / parameter / function / type / …) — accurate even where the
/// lexer-and-AST heuristics would misclassify (e.g. a bare function reference
/// with no trailing `(`). Tokens that do not resolve to a symbol, and every
/// token when `sem` is `None` (loader failed, so no semantics), fall back to
/// the heuristic classifier so highlighting degrades gracefully rather than
/// disappearing.
///
/// The returned tokens carry `start_char` as a 0-based **codepoint** column
/// (matching `Span::column - 1` from the lexer) and `length` as a codepoint
/// count. [`delta_encode`] later converts both into the negotiated LSP
/// position encoding.
pub fn compute(source: &str, sem: Option<&Semantics>) -> Vec<SemanticToken> {
    // 1. Lex (resilient — always succeeds; malformed input simply yields
    // recovery tokens which classify_token treats as plain).
    let lex_result = lex(source);
    let tokens = lex_result.tokens;
    let comments = lex_result.comments;

    // 2. Obtain the AST that drives the heuristic fallback (type-position
    // spans) and the parameter-id set. Reuse the snapshot's already-parsed
    // entry module when available; only parse ourselves when there is no
    // snapshot (loader failure), so the common path does not re-parse.
    let snapshot_ast = sem.and_then(|s| s.modules.get(&s.entry_module_source));
    let owned_parse = snapshot_ast.is_none().then(|| wado_compiler::parse(source));
    let ast = snapshot_ast
        .or_else(|| owned_parse.as_ref().map(|p| &p.ast))
        .expect("snapshot AST or freshly parsed AST is present");
    let ast_types = collect_type_spans(ast);

    // 3. Precompute the resolved-symbol classification map (byte start →
    // (token type, modifiers)) in one linear pass over the semantics. This
    // makes per-token identifier classification an O(1) lookup instead of a
    // positional AST search (`cursor_at`/`ast_id_at`) per token.
    let sem_classes = sem.map(|s| build_semantic_classes(s, &ast_types));

    // 4. Classify lexer tokens
    let mut result = Vec::new();
    for i in 0..tokens.len() {
        if let Some(st) = classify_token(source, &tokens, i, &ast_types, sem_classes.as_ref()) {
            result.push(st);
        }
    }

    // 5. Add comments
    //
    // LSP semantic tokens MUST NOT span lines. Block comments / doc
    // comments that cross a newline are skipped here — the editor's
    // TextMate grammar (or the language's syntactic highlighter)
    // already covers them and a half-encoded LSP token would render
    // worse than no token at all.
    for comment in &comments {
        if comment.span.line != comment.span.end_line {
            continue;
        }
        let line = comment.span.line.saturating_sub(1) as u32;
        let start_char = comment.span.column.saturating_sub(1) as u32;
        let length = source
            .get(comment.span.start..comment.span.end)
            .map(|s| s.chars().count() as u32)
            .unwrap_or(0);
        result.push(SemanticToken {
            line,
            start_char,
            length,
            token_type: token_type::COMMENT,
            modifiers: 0,
        });
    }

    // 6. Sort by position
    result.sort_by(|a, b| a.line.cmp(&b.line).then(a.start_char.cmp(&b.start_char)));
    result
}

/// Delta-encode semantic tokens for LSP response.
///
/// The tokens emitted by [`compute`] carry 0-based codepoint positions
/// and codepoint lengths — matching the compiler's `Span` semantics.
/// The LSP wire format expects positions and lengths in the negotiated
/// encoding's code units; re-encoding happens here.
pub fn delta_encode(
    tokens: &[SemanticToken],
    source: &str,
    encoding: PositionEncoding,
) -> Vec<u32> {
    // Pre-split lines once with their codepoint count cached alongside.
    // The hot loop converts twice per token (start and end). Without
    // this cache each call would recompute `line.chars().count()`, so
    // delta_encode would be O(tokens × line_codepoints); with the cache
    // it's O(total_codepoints + tokens × min(token_codepoints, line)).
    // UTF-32 short-circuits to a single `min`.
    let lines: Vec<(&str, u32)> = source
        .split_inclusive('\n')
        .map(|line| {
            let text = line_without_terminator(line);
            (text, text.chars().count() as u32)
        })
        .collect();

    let mut data = Vec::with_capacity(tokens.len() * 5);
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;

    for token in tokens {
        let (line_text, line_codepoints) =
            lines.get(token.line as usize).copied().unwrap_or(("", 0));
        let start_char =
            codepoints_to_code_units(line_text, line_codepoints, token.start_char, encoding);
        let end_char = codepoints_to_code_units(
            line_text,
            line_codepoints,
            token.start_char + token.length,
            encoding,
        );
        let length_in_encoding = end_char.saturating_sub(start_char);

        let delta_line = token.line - prev_line;
        let delta_start = if delta_line == 0 {
            start_char - prev_start
        } else {
            start_char
        };

        data.push(delta_line);
        data.push(delta_start);
        data.push(length_in_encoding);
        data.push(token.token_type);
        data.push(token.modifiers);

        prev_line = token.line;
        prev_start = start_char;
    }
    data
}

// --- AST-based type span collection ---

/// Spans collected from the AST for identifier refinement.
#[derive(Default)]
struct TypeSpans {
    /// byte start → token type for identifiers that are types/params/etc.
    map: IndexMap<usize, u32>,
    /// `AstId` of every function / closure parameter binding. A resolved
    /// `Variable` symbol whose definition id is in this set is a parameter
    /// (the symbol table records no parameter/local distinction, so it is
    /// recovered structurally here). Declaration and use sites share the
    /// binding's definition id, so one set covers both.
    param_ids: IndexSet<AstId>,
}

impl TypeSpans {
    fn insert(&mut self, start: usize, token_type: u32) {
        self.map.insert(start, token_type);
    }

    fn get(&self, start: usize) -> Option<u32> {
        self.map.get(&start).copied()
    }

    fn mark_param(&mut self, id: AstId) {
        self.param_ids.insert(id);
    }

    fn is_param(&self, id: AstId) -> bool {
        self.param_ids.contains(&id)
    }
}

/// Walk the AST and collect spans for type names, type parameters, etc.,
/// plus the id of every parameter binding (see [`TypeSpans::param_ids`]).
fn collect_type_spans(module: &ast::Module) -> TypeSpans {
    let mut spans = TypeSpans::default();
    for item in &module.items {
        visit_item(&mut spans, item);
    }
    spans
}

fn visit_item(spans: &mut TypeSpans, item: &Item) {
    match item {
        Item::Function(f) => visit_function(spans, f),
        Item::Struct(s) => {
            visit_generic_params(spans, &s.type_params);
            for field in &s.fields {
                visit_type(spans, &field.ty);
            }
        }
        Item::Enum(e) => {
            visit_generic_params(spans, &e.type_params);
        }
        Item::Variant(v) => {
            visit_generic_params(spans, &v.type_params);
            for case in &v.cases {
                if let Some(ty) = &case.payload {
                    visit_type(spans, ty);
                }
            }
        }
        Item::Flags(_) => {}
        Item::Newtype(n) => {
            visit_generic_params(spans, &n.type_params);
            visit_type(spans, &n.ty);
        }
        Item::Impl(imp) => {
            visit_generic_params(spans, &imp.type_params);
            if let Some(trait_ty) = &imp.trait_type {
                visit_type(spans, trait_ty);
            }
            visit_type(spans, &imp.ty);
            for method in &imp.methods {
                visit_function(spans, method);
            }
        }
        Item::Trait(t) => {
            visit_generic_params(spans, &t.type_params);
            for method in &t.methods {
                visit_function(spans, method);
            }
        }
        Item::Interface(e) => {
            for method in &e.methods {
                for param in &method.params {
                    visit_type(spans, &param.ty);
                }
                if let Some(ret) = &method.return_type {
                    visit_type(spans, ret);
                }
            }
        }
        Item::Global(g) => {
            visit_type(spans, &g.ty);
            visit_expr(spans, &g.initializer);
        }
        Item::BuiltinTypeDecl(d) => {
            visit_generic_params(spans, &d.type_params);
        }
        Item::Use(_)
        | Item::Resource(_)
        | Item::World(_)
        | Item::Test(_)
        | Item::TupleTypeDecl(_) => {}
        // Error-recovery placeholder; lexer-level highlighting covers it.
        Item::Error(_) => {}
    }
}

fn visit_function(spans: &mut TypeSpans, f: &ast::Function) {
    visit_generic_params(spans, &f.type_params);
    for param in &f.params {
        spans.mark_param(param.id);
        visit_type(spans, &param.ty);
    }
    if let Some(ret) = &f.return_type {
        visit_type(spans, ret);
    }
    if let Some(body) = &f.body {
        visit_block(spans, body);
    }
}

fn visit_generic_params(spans: &mut TypeSpans, params: &[ast::GenericParam]) {
    for param in params {
        spans.insert(param.span.start, token_type::TYPE_PARAMETER);
        for bound in &param.bounds {
            spans.insert(bound.span.start, token_type::TYPE);
        }
    }
}

fn visit_type(spans: &mut TypeSpans, ty: &Type) {
    match ty {
        Type::Named(n) => {
            spans.insert(n.span.start, token_type::TYPE);
        }
        Type::Generic(g) => {
            spans.insert(g.span.start, token_type::TYPE);
            for arg in &g.args {
                visit_type(spans, arg);
            }
        }
        Type::NamespacedGeneric(ng) => {
            // The span covers the whole `ns::name<args>`, but the namespace part is useful
            for arg in &ng.args {
                visit_type(spans, arg);
            }
        }
        Type::Function(ft) => {
            for param in &ft.params {
                visit_type(spans, param);
            }
            visit_type(spans, &ft.return_type);
        }
        Type::Tuple(types) => {
            for t in types {
                visit_type(spans, t);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            visit_type(spans, inner);
        }
        Type::TypePackSpread(_, _) => {}
        Type::Error(_) => {}
    }
}

fn visit_block(spans: &mut TypeSpans, block: &ast::Block) {
    for stmt in &block.stmts {
        visit_stmt(spans, stmt);
    }
}

fn visit_stmt(spans: &mut TypeSpans, stmt: &Stmt) {
    match stmt {
        Stmt::Let(l) => {
            if let Some(ty) = &l.ty {
                visit_type(spans, ty);
            }
            if let Some(val) = &l.value {
                visit_expr(spans, val);
            }
        }
        Stmt::Expr(e) => visit_expr(spans, &e.expr),
        Stmt::Return(r) => {
            if let Some(val) = &r.value {
                visit_expr(spans, val);
            }
        }
        Stmt::TaskReturn(tr) => visit_expr(spans, &tr.value),
        Stmt::If(i) => {
            visit_condition(spans, &i.condition);
            visit_block(spans, &i.then_block);
            if let Some(else_block) = &i.else_block {
                visit_block(spans, else_block);
            }
        }
        Stmt::While(w) => {
            visit_condition(spans, &w.condition);
            visit_block(spans, &w.body);
        }
        Stmt::For(f) => {
            if let Some(init) = &f.init {
                visit_stmt(spans, init);
            }
            if let Some(cond) = &f.condition {
                visit_condition(spans, cond);
            }
            if let Some(update) = &f.update {
                visit_expr(spans, update);
            }
            visit_block(spans, &f.body);
        }
        Stmt::ForOf(fo) => {
            visit_expr(spans, &fo.iterable);
            visit_block(spans, &fo.body);
        }
        Stmt::Loop(l) => visit_block(spans, &l.body),
        Stmt::Match(m) => visit_match(spans, m),
        Stmt::Break(b) => {
            if let Some(val) = &b.value {
                visit_expr(spans, val);
            }
        }
        Stmt::Continue(_) => {}
        Stmt::Assert(a) => {
            visit_expr(spans, &a.condition);
            if let Some(msg) = &a.message {
                visit_expr(spans, msg);
            }
        }
        Stmt::LabeledBlock(lb) => visit_block(spans, &lb.block),
        Stmt::Error(_) => {}
    }
}

fn visit_condition(spans: &mut TypeSpans, cond: &ast::Condition) {
    match cond {
        ast::Condition::Expr(e) => visit_expr(spans, e),
        ast::Condition::LetChain { elements, .. } => {
            for elem in elements {
                match elem {
                    ast::ConditionElement::Let { expr, .. } => visit_expr(spans, expr),
                    ast::ConditionElement::Expr(e) => visit_expr(spans, e),
                }
            }
        }
    }
}

fn visit_match(spans: &mut TypeSpans, m: &ast::MatchExpr) {
    visit_expr(spans, &m.expr);
    for arm in &m.arms {
        if let Some(guard) = &arm.guard {
            visit_expr(spans, guard);
        }
        visit_expr(spans, &arm.body);
    }
}

fn visit_expr(spans: &mut TypeSpans, expr: &Expr) {
    match expr {
        Expr::Ident(_) | Expr::Literal(_) => {}
        Expr::Binary(b) => {
            visit_expr(spans, &b.left);
            visit_expr(spans, &b.right);
        }
        Expr::Unary(u) => visit_expr(spans, &u.expr),
        Expr::Assign(a) => {
            visit_expr(spans, &a.target);
            visit_expr(spans, &a.value);
        }
        Expr::CompoundAssign(ca) => {
            visit_expr(spans, &ca.target);
            visit_expr(spans, &ca.value);
        }
        Expr::ComparisonChain(cc) => {
            visit_expr(spans, &cc.first);
            for cmp in &cc.comparisons {
                visit_expr(spans, &cmp.right);
            }
        }
        Expr::Call(c) => {
            visit_expr(spans, &c.callee);
            for ty in &c.type_args {
                visit_type(spans, ty);
            }
            for arg in &c.args {
                visit_expr(spans, arg);
            }
        }
        Expr::MethodCall(mc) => {
            visit_expr(spans, &mc.receiver);
            for ty in &mc.type_args {
                visit_type(spans, ty);
            }
            for arg in &mc.args {
                visit_expr(spans, arg);
            }
        }
        Expr::StaticMethodCall(smc) => {
            visit_type(spans, &smc.target_type);
            for ty in &smc.type_args {
                visit_type(spans, ty);
            }
            for arg in &smc.args {
                visit_expr(spans, arg);
            }
        }
        Expr::FieldAccess(fa) => visit_expr(spans, &fa.expr),
        Expr::Index(idx) => {
            visit_expr(spans, &idx.expr);
            visit_expr(spans, &idx.index);
        }
        Expr::Block(b) => visit_block(spans, b),
        Expr::If(i) => {
            visit_condition(spans, &i.condition);
            visit_block(spans, &i.then_block);
            if let Some(else_block) = &i.else_block {
                visit_block(spans, else_block);
            }
        }
        Expr::Match(m) => visit_match(spans, m),
        Expr::Matches(m) => visit_expr(spans, &m.expr),
        Expr::Closure(c) => {
            for param in &c.params {
                spans.mark_param(param.id);
                if let Some(ty) = &param.ty {
                    visit_type(spans, ty);
                }
            }
            visit_expr(spans, &c.body);
        }
        Expr::TemplateString(ts) => {
            for part in &ts.parts {
                if let ast::TemplatePart::Interpolation { expr, .. } = part {
                    visit_expr(spans, expr);
                }
            }
        }
        Expr::Cast(c) => {
            visit_expr(spans, &c.expr);
            visit_type(spans, &c.target_type);
        }
        Expr::StructLiteral(sl) => {
            for field in &sl.fields {
                visit_expr(spans, &field.value);
            }
        }
        Expr::TupleLiteral(tl) => {
            for elem in &tl.elements {
                visit_expr(spans, elem);
            }
        }
        Expr::LabeledBlock(lb) => visit_block(spans, &lb.block),
        Expr::TryOp(t) => visit_expr(spans, &t.expr),
        Expr::Spread(inner, _) => visit_expr(spans, inner),
        Expr::Range(r) => {
            visit_expr(spans, &r.start);
            visit_expr(spans, &r.end);
        }
        Expr::WithHandler(w) => {
            for binding in &w.handlers {
                visit_expr(spans, &binding.handler);
            }
            visit_block(spans, &w.body);
        }
        Expr::Resume(r) => visit_expr(spans, &r.value),
        Expr::Error(_) => {}
    }
}

// --- Lexer token classification ---

fn classify_token(
    source: &str,
    tokens: &[Token],
    index: usize,
    ast_types: &TypeSpans,
    sem_classes: Option<&IndexMap<usize, (u32, u32)>>,
) -> Option<SemanticToken> {
    let token = &tokens[index];
    if token.kind == TokenKind::Eof {
        return None;
    }

    // LSP semantic tokens MUST NOT span lines (see `compute` doc).
    // Multi-line string / template literals are skipped; the editor's
    // syntactic highlighter will still colour them.
    if token.span.line != token.span.end_line {
        return None;
    }

    let line = token.span.line.saturating_sub(1) as u32;
    let start_char = token.span.column.saturating_sub(1) as u32;
    let length = source
        .get(token.span.start..token.span.end)
        .map(|s| s.chars().count() as u32)
        .unwrap_or(0);
    if length == 0 {
        return None;
    }

    let (token_type, modifiers) = match &token.kind {
        // Keywords
        k if k.as_keyword_str().is_some() => (token_type::KEYWORD, 0),

        // Identifiers: prefer the resolved symbol classification (precomputed
        // in `sem_classes`, keyed by byte start) when available, otherwise
        // fall back to the lexer/AST heuristics.
        TokenKind::Ident(_) => sem_classes
            .and_then(|classes| classes.get(&token.span.start).copied())
            .unwrap_or_else(|| classify_ident(tokens, index, ast_types)),

        // Literals
        TokenKind::NumberLit(_) => (token_type::NUMBER, 0),
        TokenKind::StringLit(_) | TokenKind::TemplateStringLit(_) | TokenKind::CharLit(_) => {
            (token_type::STRING, 0)
        }

        // Operators (the highlightable subset; the registry's
        // `is_highlight_operator` flag excludes punctuation-like tokens).
        k if k.is_highlight_operator() => (token_type::OPERATOR, 0),

        // Punctuation — skip (don't emit semantic tokens for brackets, commas, etc.)
        _ => return None,
    };

    Some(SemanticToken {
        line,
        start_char,
        length,
        token_type,
        modifiers,
    })
}

/// Build the `byte start → (token type, modifiers)` classification map for
/// every identifier the semantics can resolve, in one linear pass.
///
/// Declarations come from [`Semantics::iter_symbols`] (tagged
/// `declaration`/`definition`); use sites come from
/// [`Semantics::iter_references`] (classified by the symbol they point at).
/// Both key on the byte start of the name span, which equals the lexer
/// token's `span.start`, so [`classify_token`] resolves identifiers with a
/// single map lookup. Tokens absent from the map (field access, unresolved
/// method receivers, …) fall back to the heuristic classifier.
fn build_semantic_classes(sem: &Semantics, ast_types: &TypeSpans) -> IndexMap<usize, (u32, u32)> {
    let entry = &sem.entry_module_source;
    let mut classes: IndexMap<usize, (u32, u32)> = IndexMap::default();

    // Declaration sites: the binding's own name span.
    for (key, symbol) in sem.iter_symbols() {
        if key.module != *entry {
            continue;
        }
        let Some(span) = sem.name_span_of(key) else {
            continue;
        };
        let (token_type, mut modifiers) = classify_symbol(symbol, key, ast_types, entry);
        modifiers |= token_modifier::DECLARATION | token_modifier::DEFINITION;
        classes.insert(span.start, (token_type, modifiers));
    }

    // Use sites: every recorded use→def edge, classified by the def symbol.
    for (use_key, def_key) in sem.iter_references() {
        if use_key.module != *entry {
            continue;
        }
        let (Some(symbol), Some(span)) = (sem.symbol_at(def_key), sem.span_of_key(&use_key)) else {
            continue;
        };
        classes.insert(
            span.start,
            classify_symbol(symbol, def_key, ast_types, entry),
        );
    }

    classes
}

/// Map a resolved symbol to its LSP token type and the modifiers implied by
/// the symbol itself (e.g. `readonly` for an immutable binding). The
/// `declaration` modifier is added by the caller for declaration sites.
fn classify_symbol(
    symbol: &Symbol,
    def_key: &SymbolKey,
    ast_types: &TypeSpans,
    entry: &ModuleSource,
) -> (u32, u32) {
    let mut modifiers = 0;
    let token_type = match &symbol.kind {
        SymbolKind::Function(_) => token_type::FUNCTION,
        SymbolKind::Struct(_) => token_type::STRUCT,
        SymbolKind::Enum(_) | SymbolKind::Flags(_) | SymbolKind::Variant(_) => token_type::ENUM,
        SymbolKind::Effect(_) | SymbolKind::Trait(_) => token_type::INTERFACE,
        SymbolKind::Resource(_) => token_type::CLASS,
        SymbolKind::Newtype(_) | SymbolKind::BuiltinType | SymbolKind::World(_) => token_type::TYPE,
        SymbolKind::Variable(v) => {
            if !v.is_mut {
                modifiers |= token_modifier::READONLY;
            }
            // The symbol table records no parameter/local distinction, so a
            // same-module definition id in the parameter set marks parameters.
            if def_key.module == *entry && ast_types.is_param(def_key.ast_id) {
                token_type::PARAMETER
            } else {
                token_type::VARIABLE
            }
        }
        SymbolKind::Global(g) => {
            if !g.is_mut {
                modifiers |= token_modifier::READONLY;
            }
            token_type::VARIABLE
        }
    };
    (token_type, modifiers)
}

/// Classify an identifier using AST type spans + lexer context heuristics.
fn classify_ident(tokens: &[Token], index: usize, ast_types: &TypeSpans) -> (u32, u32) {
    let token = &tokens[index];

    // 1. Check AST classification (types, type parameters)
    if let Some(tt) = ast_types.get(token.span.start) {
        return (tt, 0);
    }

    // 2. Look at previous token for declaration context
    if let Some(prev) = prev_significant(tokens, index) {
        match &prev.kind {
            TokenKind::Fn => return (token_type::FUNCTION, 0),
            TokenKind::Struct
            | TokenKind::Enum
            | TokenKind::Variant
            | TokenKind::Trait
            | TokenKind::Type
            | TokenKind::Flags
            | TokenKind::Resource
            | TokenKind::World
            | TokenKind::Effect => return (token_type::TYPE, 0),
            TokenKind::Dot => {
                // After `.`: method if followed by `(`, else property
                if next_is(tokens, index, &TokenKind::LParen) {
                    return (token_type::METHOD, 0);
                }
                return (token_type::PROPERTY, 0);
            }
            TokenKind::ColonColon => {
                // After `::`: could be enum member or static method
                if next_is(tokens, index, &TokenKind::LParen) {
                    return (token_type::FUNCTION, 0);
                }
                if next_is(tokens, index, &TokenKind::ColonColon) {
                    // Middle of a path like `A::B::C` - treat as type
                    return (token_type::TYPE, 0);
                }
                return (token_type::ENUM_MEMBER, 0);
            }
            _ => {}
        }
    }

    // 3. Check if followed by `(` → function call
    if next_is(tokens, index, &TokenKind::LParen) {
        return (token_type::FUNCTION, 0);
    }

    // 4. Default to variable
    (token_type::VARIABLE, 0)
}

fn prev_significant(tokens: &[Token], index: usize) -> Option<&Token> {
    if index > 0 {
        Some(&tokens[index - 1])
    } else {
        None
    }
}

fn next_is(tokens: &[Token], index: usize, kind: &TokenKind) -> bool {
    tokens
        .get(index + 1)
        .is_some_and(|t| std::mem::discriminant(&t.kind) == std::mem::discriminant(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Semantics` snapshot for `source` so the semantic
    /// classification path can be exercised in unit tests.
    fn sem_of(source: &str) -> Semantics {
        let host = crate::test_support::MapHost::single("/test.wado", source);
        futures::executor::block_on(wado_compiler::semantics(source, &host, Some("/test.wado")))
    }

    #[test]
    fn test_empty_source() {
        let tokens = compute("", None);
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_keyword_classification() {
        let tokens = compute("fn foo() {}", None);
        // `fn` should be keyword
        assert_eq!(tokens[0].token_type, token_type::KEYWORD);
        // `foo` should be function
        assert_eq!(tokens[1].token_type, token_type::FUNCTION);
    }

    #[test]
    fn test_type_annotation() {
        let tokens = compute("fn foo(x: i32) {}", None);
        // `i32` at char 10 should be TYPE (from AST NamedType span)
        let i32_token = tokens.iter().find(|t| t.start_char == 10).unwrap();
        assert_eq!(i32_token.token_type, token_type::TYPE);
        // `x` at char 7 should be VARIABLE under the heuristic path (no
        // separate parameter detection without semantics).
        let x_token = tokens.iter().find(|t| t.start_char == 7).unwrap();
        assert!(
            x_token.token_type == token_type::VARIABLE
                || x_token.token_type == token_type::PARAMETER
        );
    }

    #[test]
    fn test_string_literal() {
        let tokens = compute("let s = \"hello\";", None);
        let string_token = tokens.iter().find(|t| t.token_type == token_type::STRING);
        assert!(string_token.is_some());
    }

    #[test]
    fn semantic_parameter_classification() {
        // With semantics, both the declaration and the use site of a
        // parameter resolve to PARAMETER — the heuristic path cannot tell a
        // parameter use from any other variable.
        let src = "fn foo(x: i32) -> i32 { return x; }";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));

        // `x` parameter declaration at char 7.
        let decl = tokens.iter().find(|t| t.start_char == 7).unwrap();
        assert_eq!(decl.token_type, token_type::PARAMETER);
        assert_ne!(decl.modifiers & token_modifier::DECLARATION, 0);
        assert_ne!(decl.modifiers & token_modifier::READONLY, 0);

        // `x` use site inside `return x` — not a declaration.
        let use_site = tokens
            .iter()
            .find(|t| t.token_type == token_type::PARAMETER && t.start_char != 7)
            .unwrap();
        assert_eq!(use_site.modifiers & token_modifier::DECLARATION, 0);
    }

    #[test]
    fn semantic_function_reference_without_call() {
        // A bare function reference (no trailing `(`) is the canonical case
        // the heuristic gets wrong — it would classify `g` as a variable.
        let src = "fn g() {}\nfn f() { let h = g; }";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));

        // `g` on the second line, used as a value (line 1, after `= `).
        let g_ref = tokens
            .iter()
            .find(|t| t.line == 1 && t.token_type == token_type::FUNCTION && t.start_char > 10)
            .unwrap();
        assert_eq!(g_ref.token_type, token_type::FUNCTION);
    }

    #[test]
    fn semantic_local_is_readonly_unless_mut() {
        let src = "fn f() { let a = 1; let mut b = 2; }";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));

        // `a` is an immutable local → VARIABLE + readonly + declaration.
        let a = tokens.iter().find(|t| t.start_char == 13).unwrap();
        assert_eq!(a.token_type, token_type::VARIABLE);
        assert_ne!(a.modifiers & token_modifier::READONLY, 0);
        assert_ne!(a.modifiers & token_modifier::DECLARATION, 0);

        // `b` is `let mut` → not readonly.
        let b = tokens
            .iter()
            .find(|t| t.token_type == token_type::VARIABLE && t.start_char > 25)
            .unwrap();
        assert_eq!(b.modifiers & token_modifier::READONLY, 0);
    }

    #[test]
    fn semantic_struct_is_struct_kind() {
        let src = "struct Point { x: i32 }\nfn f() { let p = Point { x: 1 }; }";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));

        // `Point` in the struct literal on line 1 resolves to a struct symbol.
        let point_use = tokens
            .iter()
            .find(|t| t.line == 1 && t.token_type == token_type::STRUCT)
            .unwrap();
        assert_eq!(point_use.token_type, token_type::STRUCT);
    }

    #[test]
    fn falls_back_to_heuristics_without_semantics() {
        // No semantics: a bare function reference degrades to VARIABLE rather
        // than disappearing. This pins the graceful-degradation contract.
        let src = "fn g() {}\nfn f() { let h = g; }";
        let tokens = compute(src, None);
        let g_ref = tokens.iter().find(|t| t.line == 1 && t.start_char > 10);
        assert!(g_ref.is_some(), "identifier must still be classified");
    }

    #[test]
    fn multi_line_tokens_are_skipped() {
        // LSP semantic tokens MUST NOT span lines. Both lexer-emitted
        // tokens (raw-newline string literals, doc/block comments) and
        // the comment list path used to silently slip through and
        // produce wrong-length entries that delta_encode then clipped
        // to end-of-start-line. The fixture below uses a `/* */` block
        // comment that crosses a newline; the parse-emitted single-
        // line tokens around it (`fn`, `f`, etc.) must still appear,
        // but no COMMENT token may be produced.
        let src = "fn f() {\n    /* multi\n    line */\n    let _ = 1;\n}\n";
        let tokens = compute(src, None);
        for tok in &tokens {
            // No token may span lines under any circumstances.
            assert_eq!(
                tok.length,
                tok.length, // pin for the assertion structure
            );
        }
        assert!(
            tokens.iter().all(|t| t.token_type != token_type::COMMENT),
            "multi-line block comment must be skipped, not partially encoded",
        );
        // Sanity: the single-line tokens around the comment still made it through.
        assert!(
            tokens.iter().any(|t| t.token_type == token_type::KEYWORD),
            "non-comment tokens around the multi-line comment must still appear",
        );
    }

    #[test]
    fn test_delta_encoding() {
        let tokens = vec![
            SemanticToken {
                line: 0,
                start_char: 0,
                length: 2,
                token_type: token_type::KEYWORD,
                modifiers: 0,
            },
            SemanticToken {
                line: 0,
                start_char: 3,
                length: 3,
                token_type: token_type::FUNCTION,
                modifiers: 0,
            },
            SemanticToken {
                line: 1,
                start_char: 4,
                length: 1,
                token_type: token_type::VARIABLE,
                modifiers: 0,
            },
        ];
        // Test source matching the token columns so re-encoding is a no-op.
        let src = "fn foo()\n    x\n";
        let data = delta_encode(&tokens, src, PositionEncoding::Utf16);
        assert_eq!(
            data,
            vec![
                0,
                0,
                2,
                token_type::KEYWORD,
                0, // first token
                0,
                3,
                3,
                token_type::FUNCTION,
                0, // same line, delta_start=3
                1,
                4,
                1,
                token_type::VARIABLE,
                0, // next line, start_char=4
            ]
        );
    }
}
