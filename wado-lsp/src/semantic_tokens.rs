use indexmap::{IndexMap, IndexSet};
use wado_compiler::ast::{self, AstId, AstVisitor, Expr, Item, Type};
use wado_compiler::lexer::lex;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::semantics::Semantics;
use wado_compiler::symbol::{Symbol, SymbolKind};
use wado_compiler::token::{Token, TokenKind};

use crate::text::{LineIndex, PositionEncoding};

/// LSP semantic token type indices — positions into [`TOKEN_TYPES`], the
/// legend the server advertises at `initialize`. The correspondence is
/// asserted by `legend_matches_token_type_indices`.
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

/// LSP semantic token modifier bits — bit `n` is [`TOKEN_MODIFIERS`]`[n]`.
/// Asserted by `legend_matches_token_modifier_bits`.
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
    let ast_spans = collect_ast_spans(ast);

    // 3. Precompute the resolved-symbol classification map (byte start →
    // (token type, modifiers)) in one linear pass over the semantics. This
    // makes per-token identifier classification an O(1) lookup instead of a
    // positional AST search (`cursor_at`/`ast_id_at`) per token.
    let sem_classes = sem.map(|s| build_semantic_classes(s, &ast_spans));

    // 4. Classify lexer tokens
    let mut result = Vec::new();
    for i in 0..tokens.len() {
        if let Some(st) = classify_token(source, &tokens, i, &ast_spans, sem_classes.as_ref()) {
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
    let lines = LineIndex::new(source);

    let mut data = Vec::with_capacity(tokens.len() * 5);
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;

    for token in tokens {
        let start_char = lines.to_character(token.line, token.start_char, encoding);
        let end_char = lines.to_character(token.line, token.start_char + token.length, encoding);
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

// --- AST-based span collection ---

/// Per-identifier classification the lexer alone cannot produce, collected in
/// one [`AstVisitor`] pass over the entry module.
#[derive(Default)]
struct AstSpans {
    /// byte start -> token type for identifiers the AST classifies: type
    /// names, type parameters, and the contextual keywords.
    map: IndexMap<usize, u32>,
    /// `AstId` of every function / closure parameter binding. A resolved
    /// `Variable` symbol whose definition id is in this set is a parameter
    /// (the symbol table records no parameter/local distinction, so it is
    /// recovered structurally here). Declaration and use sites share the
    /// binding's definition id, so one set covers both.
    param_ids: IndexSet<AstId>,
}

impl AstSpans {
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

/// Collect the AST-derived classifications for `module`.
///
/// Traversal is [`AstVisitor`]'s, so a node a later AST change adds is reached
/// by construction rather than silently skipped.
fn collect_ast_spans(module: &ast::Module) -> AstSpans {
    let mut collector = SpanCollector::default();
    for item in &module.items {
        collector.visit_item(item);
    }
    collector.spans
}

#[derive(Default)]
struct SpanCollector {
    spans: AstSpans,
}

impl AstVisitor for SpanCollector {
    fn visit_item(&mut self, item: &Item) {
        // `test` is a contextual keyword: it lexes as an identifier, so the
        // declaration's own span is the only place it can be recognised.
        if let Item::Test(t) = item {
            self.spans.insert(t.span.start, token_type::KEYWORD);
        }
        ast::walk_item(self, item);
    }

    fn visit_function(&mut self, func: &ast::Function) {
        for param in &func.params {
            self.spans.mark_param(param.id);
        }
        ast::walk_function(self, func);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Closure(c) = expr {
            for param in &c.params {
                self.spans.mark_param(param.id);
            }
        }
        // `resume` and `do`, the other two contextual keywords.
        if let Expr::Resume(r) = expr {
            self.spans.insert(r.span.start, token_type::KEYWORD);
        }
        if let Expr::WithHandler(w) = expr {
            self.spans.insert(w.do_span.start, token_type::KEYWORD);
        }
        ast::walk_expr(self, expr);
    }

    fn visit_generic_params(&mut self, params: &[ast::GenericParam]) {
        for param in params {
            self.spans
                .insert(param.span.start, token_type::TYPE_PARAMETER);
        }
        ast::walk_generic_params(self, params);
    }

    fn visit_trait_bounds(&mut self, bounds: &[ast::TraitBound]) {
        for bound in bounds {
            self.spans.insert(bound.span.start, token_type::TYPE);
        }
        ast::walk_trait_bounds(self, bounds);
    }

    fn visit_type(&mut self, ty: &Type) {
        match ty {
            Type::Named(t) => self.spans.insert(t.span.start, token_type::TYPE),
            Type::Generic(t) => self.spans.insert(t.span.start, token_type::TYPE),
            // `ns::Value` and `Self::Item` parse to the same node, so the head
            // is not knowably a namespace; leave it to the symbol map.
            Type::NamespacedGeneric(_)
            | Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _)
            | Type::Infer(_)
            | Type::Error(_) => {}
        }
        ast::walk_type(self, ty);
    }
}

// --- Lexer token classification ---

fn classify_token(
    source: &str,
    tokens: &[Token],
    index: usize,
    ast_spans: &AstSpans,
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
            .unwrap_or_else(|| classify_ident(tokens, index, ast_spans)),

        // Literals
        TokenKind::NumberLit(_) => (token_type::NUMBER, 0),
        TokenKind::StringLit(_)
        | TokenKind::ByteStringLit(_)
        | TokenKind::TemplateStringLit(_)
        | TokenKind::CharLit(_) => (token_type::STRING, 0),

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
fn build_semantic_classes(sem: &Semantics, ast_spans: &AstSpans) -> IndexMap<usize, (u32, u32)> {
    let entry = &sem.entry_module_source;
    let mut classes: IndexMap<usize, (u32, u32)> = IndexMap::default();

    // Declaration sites: the binding's own name span.
    for (id, symbol) in sem.iter_symbols() {
        if sem.module_of_id(id) != Some(entry) {
            continue;
        }
        let Some(span) = sem.name_span_of(id) else {
            continue;
        };
        let (token_type, mut modifiers) = classify_symbol(symbol, id, ast_spans, sem, entry);
        modifiers |= token_modifier::DECLARATION | token_modifier::DEFINITION;
        classes.insert(span.start, (token_type, modifiers));
    }

    // Use sites: every recorded use→def edge, classified by the def symbol.
    for (use_id, def_id) in sem.iter_references() {
        if sem.module_of_id(use_id) != Some(entry) {
            continue;
        }
        let (Some(symbol), Some(span)) = (sem.symbol_at(def_id), sem.span_of_id(use_id)) else {
            continue;
        };
        classes.insert(
            span.start,
            classify_symbol(symbol, def_id, ast_spans, sem, entry),
        );
    }

    classes
}

/// Map a resolved symbol to its LSP token type and the modifiers implied by
/// the symbol itself (e.g. `readonly` for an immutable binding). The
/// `declaration` modifier is added by the caller for declaration sites.
fn classify_symbol(
    symbol: &Symbol,
    def_id: AstId,
    ast_spans: &AstSpans,
    sem: &Semantics,
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
            if sem.module_of_id(def_id) == Some(entry) && ast_spans.is_param(def_id) {
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
fn classify_ident(tokens: &[Token], index: usize, ast_spans: &AstSpans) -> (u32, u32) {
    let token = &tokens[index];

    // 1. Check AST classification (types, type parameters)
    if let Some(tt) = ast_spans.get(token.span.start) {
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

    /// Every `token_type` constant indexes its own name in the legend.
    ///
    /// A constant one slot off still compiles and still carries *a* type
    /// through every behavioural test; it shows up only as wrongly coloured
    /// code in the editor.
    #[test]
    fn legend_matches_token_type_indices() {
        let expected = [
            (token_type::NAMESPACE, "namespace"),
            (token_type::TYPE, "type"),
            (token_type::TYPE_PARAMETER, "typeParameter"),
            (token_type::PARAMETER, "parameter"),
            (token_type::VARIABLE, "variable"),
            (token_type::PROPERTY, "property"),
            (token_type::ENUM_MEMBER, "enumMember"),
            (token_type::FUNCTION, "function"),
            (token_type::METHOD, "method"),
            (token_type::KEYWORD, "keyword"),
            (token_type::COMMENT, "comment"),
            (token_type::STRING, "string"),
            (token_type::NUMBER, "number"),
            (token_type::OPERATOR, "operator"),
            (token_type::STRUCT, "struct"),
            (token_type::ENUM, "enum"),
            (token_type::INTERFACE, "interface"),
            (token_type::CLASS, "class"),
        ];
        assert_eq!(
            expected.len(),
            TOKEN_TYPES.len(),
            "every legend entry needs a named constant (and vice versa)",
        );
        for (index, name) in expected {
            assert_eq!(
                TOKEN_TYPES.get(index as usize),
                Some(&name),
                "token type {index} should be {name:?}",
            );
        }
    }

    /// Each modifier constant is one bit, positioned at its legend name.
    #[test]
    fn legend_matches_token_modifier_bits() {
        let expected = [
            (token_modifier::DECLARATION, "declaration"),
            (token_modifier::DEFINITION, "definition"),
            (token_modifier::READONLY, "readonly"),
        ];
        assert_eq!(expected.len(), TOKEN_MODIFIERS.len());
        for (bit, name) in expected {
            assert_eq!(
                bit.count_ones(),
                1,
                "{name} must be a single bit, got {bit}"
            );
            assert_eq!(
                TOKEN_MODIFIERS.get(bit.trailing_zeros() as usize),
                Some(&name),
                "modifier bit {bit} should be {name:?}",
            );
        }
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
        // A partially encoded token renders worse than none.
        let src = "fn f() {\n    /* multi\n    line */\n    let _ = 1;\n}\n";
        let tokens = compute(src, None);
        // "Must not span lines", expressed against a start and a length.
        let line_lengths: Vec<u32> = src
            .split_inclusive('\n')
            .map(|l| crate::text::line_without_terminator(l).chars().count() as u32)
            .collect();
        for tok in &tokens {
            let line_len = line_lengths
                .get(tok.line as usize)
                .copied()
                .unwrap_or_else(|| panic!("token on a line past EOF: {tok:?}"));
            assert!(
                tok.start_char + tok.length <= line_len,
                "token runs past the end of line {}: {tok:?} (line is {line_len} codepoints)",
                tok.line,
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

    /// Classification of the token whose text is `needle` on line `line`.
    fn kind_of(tokens: &[SemanticToken], src: &str, line: u32, needle: &str) -> u32 {
        let text = src.split_inclusive('\n').nth(line as usize).expect("line");
        let col = text.find(needle).expect("needle on line") as u32;
        tokens
            .iter()
            .find(|t| t.line == line && t.start_char == col)
            .unwrap_or_else(|| panic!("no token for {needle:?} at {line}:{col}"))
            .token_type
    }

    #[test]
    fn resource_method_signature_types_are_types() {
        // `Item::Resource` was unreachable from the crate's own AST walk, so
        // nothing in a resource method's signature reached `visit_type`. Asserted
        // without semantics: the AST walk is the only thing that can classify
        // these, so a regression cannot be masked by the symbol map.
        let src = "resource R {\n    fn m(&self, count: Wide) -> Tall;\n}\nfn run() {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "Wide"), token_type::TYPE);
        assert_eq!(kind_of(&tokens, src, 1, "Tall"), token_type::TYPE);
    }

    #[test]
    fn test_block_closure_parameter_is_a_parameter() {
        // Same gap for `Item::Test`: nothing inside a test block was walked.
        let src =
            "fn run() {}\ntest \"t\" {\n    let g = |count: i32| count;\n    let _ = g(1);\n}\n";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));
        assert_eq!(kind_of(&tokens, src, 2, "count"), token_type::PARAMETER);
    }

    #[test]
    fn struct_field_default_expression_is_walked() {
        // `field.default` is an expression the compiler's walk reaches; the
        // hand-rolled copy stopped at the field type.
        let src = "struct S { n: i32 = C }\nglobal C: i32 = 1;\nfn run() {}\n";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));
        assert_eq!(kind_of(&tokens, src, 0, "i32"), token_type::TYPE);
    }

    #[test]
    fn contextual_keyword_test_is_a_keyword() {
        // `test` / `do` / `resume` lex as identifiers (they are contextual),
        // so only the AST can say they are keywords.
        let src = "fn run() {}\ntest \"t\" {\n    let _ = 1;\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "test"), token_type::KEYWORD);
    }

    #[test]
    fn contextual_keywords_do_and_resume_are_keywords() {
        let src = concat!(
            "effect E {\n",
            "    fn ask() -> i32;\n",
            "}\n",
            "struct H {}\n",
            "impl E for H {\n",
            "    fn ask() -> i32 { resume 1; }\n",
            "}\n",
            "fn run() -> i32 {\n",
            "    let h = H {};\n",
            "    return with E => h do { E::ask() };\n",
            "}\n",
        );
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 5, "resume"), token_type::KEYWORD);
        assert_eq!(kind_of(&tokens, src, 9, "do"), token_type::KEYWORD);
    }

    #[test]
    fn associated_type_qualifier_is_not_a_namespace() {
        // `Self::Item` and `json::Value` parse to the same node, so the AST
        // cannot call the head a namespace — `Self` is a type.
        let src = "trait T {\n    type Item;\n    fn get(&self) -> Self::Item<i32>;\n}\n";
        let tokens = compute(src, None);
        assert_ne!(kind_of(&tokens, src, 2, "Self"), token_type::NAMESPACE);
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
