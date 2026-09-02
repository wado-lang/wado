use indexmap::{IndexMap, IndexSet};
use wado_compiler::ast::{self, AstId, AstVisitor, Expr, Item, Type};
use wado_compiler::lexer::{lex, lex_interpolation};
use wado_compiler::module_source::ModuleSource;
use wado_compiler::semantics::Semantics;
use wado_compiler::symbol::{Symbol, SymbolKind};
use wado_compiler::syntax::KeywordCategory;
use wado_compiler::token::{Position, Span, TemplateTokenPart, Token, TokenKind};

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
    pub const DEFAULT_LIBRARY: u32 = 1 << 3;
}

/// Token modifier legend for LSP capability declaration.
pub const TOKEN_MODIFIERS: &[&str] = &["declaration", "definition", "readonly", "defaultLibrary"];

/// A language constant: `true` / `false` / `null` / `self`, the words
/// `wado_compiler::syntax` files under [`KeywordCategory::Constant`].
///
/// LSP has no `constant` token type. `variable` + `readonly` +
/// `defaultLibrary` is its standard spelling for one, and is what editors map
/// onto a constant scope — matching the `constant.language.wado` the
/// `TextMate` grammar gives the same words.
const CONSTANT: (u32, u32) = (
    token_type::VARIABLE,
    token_modifier::READONLY | token_modifier::DEFAULT_LIBRARY,
);

/// A keyword, real or contextual.
const KEYWORD: (u32, u32) = (token_type::KEYWORD, 0);

/// A classified token keyed by byte span — the classification before LSP's
/// wire constraints narrow it. See [`classify_all`].
#[derive(Debug, Clone, Copy)]
pub struct ClassifiedToken {
    pub span: Span,
    pub token_type: u32,
    pub modifiers: u32,
}

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
///
/// Multi-line tokens are dropped: LSP semantic tokens MUST NOT span lines, so
/// a block comment or a multi-line string is left to the editor's syntactic
/// highlighter rather than half-encoded. That is a wire constraint, not a
/// classification verdict — [`classify_all`] keeps them.
pub fn compute(source: &str, sem: Option<&Semantics>) -> Vec<SemanticToken> {
    classify_all(source, sem)
        .into_iter()
        .filter(|token| token.span.line == token.span.end_line)
        .map(|token| SemanticToken {
            line: token.span.line.saturating_sub(1) as u32,
            start_char: token.span.column.saturating_sub(1) as u32,
            length: source
                .get(token.span.start..token.span.end)
                .map(|text| text.chars().count() as u32)
                .unwrap_or(0),
            token_type: token.token_type,
            modifiers: token.modifiers,
        })
        .collect()
}

/// Classify every token in `source`, keyed by byte span and in source order.
///
/// The classification [`compute`] narrows to what the LSP wire accepts. Use
/// this where the whole verdict matters — comparing the compiler's own
/// highlighting against another implementation's, where a dropped multi-line
/// token reads as a disagreement.
pub fn classify_all(source: &str, sem: Option<&Semantics>) -> Vec<ClassifiedToken> {
    // Resilient: malformed input yields recovery tokens, which classify as plain.
    let lex_result = lex(source);
    let tokens = lex_result.tokens;
    let comments = lex_result.comments;

    // Reuse the snapshot's parse when there is one, so the common path does not
    // lex and parse the entry source twice.
    let snapshot_ast = sem.and_then(|s| s.modules.get(&s.entry_module_source));
    let owned_parse = snapshot_ast.is_none().then(|| wado_compiler::parse(source));
    let ast = snapshot_ast
        .or_else(|| owned_parse.as_ref().map(|p| &p.ast))
        .expect("snapshot AST or freshly parsed AST is present");
    let ast_spans = collect_ast_spans(ast);

    // One linear pass, so per-token classification is a lookup rather than a
    // positional AST search.
    let sem_classes = sem.map(|s| build_semantic_classes(s, &ast_spans));

    let mut result = Vec::new();
    for i in 0..tokens.len() {
        classify_into(&tokens, i, &ast_spans, sem_classes.as_ref(), &mut result);
    }
    for comment in &comments {
        result.push(ClassifiedToken {
            span: comment.span,
            token_type: token_type::COMMENT,
            modifiers: 0,
        });
    }
    // A `__DATA__` tail is not Wado, so no token carries it. Muting it says so.
    // In an editor the run spans lines and `compute` drops it, leaving the
    // `TextMate` grammar's embedded-JSON highlighting in place.
    if let Some(span) = lex_result.data_section_span {
        result.push(ClassifiedToken {
            span,
            token_type: token_type::COMMENT,
            modifiers: 0,
        });
    }

    result.sort_by_key(|token| token.span.start);
    result
}

/// Classify one token into `out`. A template literal expands into several;
/// everything else contributes at most one.
fn classify_into(
    tokens: &[Token],
    index: usize,
    ast_spans: &AstSpans,
    sem_classes: Option<&IndexMap<usize, (u32, u32)>>,
    out: &mut Vec<ClassifiedToken>,
) {
    let token = &tokens[index];
    if let TokenKind::TemplateStringLit(parts) = &token.kind {
        expand_template(token, parts, ast_spans, sem_classes, out);
        return;
    }
    if let Some((token_type, modifiers)) = classify_token(tokens, index, ast_spans, sem_classes) {
        out.push(ClassifiedToken {
            span: token.span,
            token_type,
            modifiers,
        });
    }
}

/// Split a template literal, which reaches the classifier as a single token —
/// so without this `${count + 1}` is string, left to the editor's syntactic
/// highlighter.
///
/// Everything outside an interpolation is string, a `:spec` tail mutes as a
/// comment, and the expression classifies like any other code:
/// [`wado_compiler::lexer::lex_interpolation`] is the call the parser makes to
/// build the AST, so the spans already sit on the file and the symbol map,
/// keyed by byte start, resolves them for free.
fn expand_template(
    token: &Token,
    parts: &[TemplateTokenPart],
    ast_spans: &AstSpans,
    sem_classes: Option<&IndexMap<usize, (u32, u32)>>,
    out: &mut Vec<ClassifiedToken>,
) {
    let (mut cursor, end) = token.span.bounds();
    for part in parts {
        let (Some((expr_start, expr_end)), TemplateTokenPart::Interpolation { expr, origin, .. }) =
            (part.expr_bounds(), part)
        else {
            continue;
        };
        push_run(out, cursor, expr_start, token_type::STRING);
        let inner = lex_interpolation(expr, *origin, token.span.space);
        for i in 0..inner.tokens.len() {
            // A template nested in this one expands the same way.
            classify_into(&inner.tokens, i, ast_spans, sem_classes, out);
        }
        // `${ /* why */ x }` — the lexer keeps comments out of the token
        // stream, so without this the comment falls through every span.
        for comment in &inner.comments {
            out.push(ClassifiedToken {
                span: comment.span,
                token_type: token_type::COMMENT,
                modifiers: 0,
            });
        }
        cursor = expr_end;
        // Left to the token stream the `>` in `${x:>8}` would colour as an
        // operator, which is what the `TextMate` grammar — regex-only, so it
        // cannot parse a specifier — still does.
        if let Some((spec_start, spec_end)) = part.format_bounds() {
            push_run(out, spec_start, spec_end, token_type::COMMENT);
            cursor = spec_end;
        }
    }
    push_run(out, cursor, end, token_type::STRING);
}

/// Emit a run of one class, unless it is empty — an interpolation can sit
/// flush against the one before it (`${a}${b}`), and against the backtick.
fn push_run(out: &mut Vec<ClassifiedToken>, from: Position, to: Position, class: u32) {
    if from.offset >= to.offset {
        return;
    }
    out.push(ClassifiedToken {
        span: from.span_to(to),
        token_type: class,
        modifiers: 0,
    });
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
    /// byte start -> class for the names the AST settles outright: the
    /// contextual keywords, and the field names. Apart from `map` because
    /// these outrank symbol resolution rather than being refined by it — a
    /// shorthand `{ state }` resolves to the binding it reads, and the symbol
    /// winning would colour it `variable` wherever a snapshot exists.
    overrides: IndexMap<usize, (u32, u32)>,
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

    fn mark_override(&mut self, start: usize, class: (u32, u32)) {
        self.overrides.insert(start, class);
    }

    fn override_at(&self, start: usize) -> Option<(u32, u32)> {
        self.overrides.get(&start).copied()
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

impl SpanCollector {
    /// An effect named in a `with` clause. The declaration site already reads
    /// as a type (`effect E`); its uses are the same name.
    fn mark_effect_names(&mut self, effects: &[(AstId, Span)]) {
        for (_, span) in effects {
            self.spans.insert(span.start, token_type::TYPE);
        }
    }

    /// Mark every key of an attribute object, and of the objects nested in it —
    /// under an array element as much as under another key.
    fn mark_attr_keys(&mut self, object: &ast::AttrObject) {
        for entry in object.values() {
            self.mark_field_name(entry.key_span);
            self.mark_attr_value(&entry.value);
        }
    }

    fn mark_attr_value(&mut self, value: &ast::AttrValue) {
        match value {
            ast::AttrValue::Object(nested) => self.mark_attr_keys(nested),
            ast::AttrValue::Array(items) => {
                for item in items {
                    self.mark_attr_value(item);
                }
            }
            ast::AttrValue::String(_)
            | ast::AttrValue::Int(_)
            | ast::AttrValue::Float(_)
            | ast::AttrValue::Bool(_) => {}
        }
    }

    fn mark_field_name(&mut self, span: Span) {
        self.spans
            .mark_override(span.start, (token_type::PROPERTY, 0));
    }
}

impl AstVisitor for SpanCollector {
    fn visit_item(&mut self, item: &Item) {
        // The contextual keywords lex as identifiers, so the declaration's own
        // span is the only place each can be recognised.
        if let Item::Test(t) = item {
            self.spans.mark_override(t.span.start, KEYWORD);
        }
        if let Item::Impl(b) = item
            && let Some(rest) = b.rest
        {
            self.spans.mark_override(rest.keyword_span.start, KEYWORD);
        }
        // Attribute keys are not expressions, so no other walk reaches them.
        if let Item::Use(decl) = item
            && let Some(attributes) = &decl.attributes
        {
            self.mark_attr_keys(&attributes.entries);
        }
        ast::walk_item(self, item);
    }

    fn visit_function(&mut self, func: &ast::Function) {
        for param in &func.params {
            self.spans.mark_param(param.id);
        }
        self.mark_effect_names(&func.effect_ids);
        ast::walk_function(self, func);
    }

    fn visit_stmt(&mut self, stmt: &ast::Stmt) {
        // `task` in `task return expr;` — the statement span opens on it.
        if let ast::Stmt::TaskReturn(t) = stmt {
            self.spans.mark_override(t.span.start, KEYWORD);
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Closure(c) = expr {
            for param in &c.params {
                self.spans.mark_param(param.id);
            }
        }
        // `resume` and `do`, two more contextual keywords.
        if let Expr::Resume(r) = expr {
            self.spans.mark_override(r.span.start, KEYWORD);
        }
        if let Expr::WithHandler(w) = expr {
            self.spans.mark_override(w.do_span.start, KEYWORD);
        }
        if let Expr::StructLiteral(literal) = expr {
            for field in &literal.fields {
                self.mark_field_name(field.name_span);
            }
        }
        ast::walk_expr(self, expr);
    }

    fn visit_pattern(&mut self, pat: &ast::Pattern) {
        // A field's span starts at its name, in `x: inner` as much as `x`.
        if let ast::Pattern::Struct { fields, .. } = pat {
            for field in fields {
                self.mark_field_name(field.span);
            }
        }
        ast::walk_pattern(self, pat);
    }

    fn visit_generic_params(&mut self, params: &[ast::GenericParam]) {
        for param in params {
            // `name_span`, not `span`: a pack parameter's `span` starts at its
            // `..`, and classification keys on the identifier's own offset.
            self.spans
                .insert(param.name_span.start, token_type::TYPE_PARAMETER);
        }
        ast::walk_generic_params(self, params);
    }

    fn visit_trait_bounds(&mut self, bounds: &[ast::TraitBound]) {
        for bound in bounds {
            self.spans.insert(bound.span.start, token_type::TYPE);
            // `Output` in `Builder<Output = T>` names an associated type.
            for assoc in &bound.assoc_types {
                self.spans.insert(assoc.span.start, token_type::TYPE);
            }
            // A closure bound's own `with` clause: `F: fn() with Stdout`.
            if let Some(signature) = &bound.fn_signature {
                self.mark_effect_names(&signature.effect_ids);
            }
        }
        ast::walk_trait_bounds(self, bounds);
    }

    fn visit_type(&mut self, ty: &Type) {
        match ty {
            Type::Named(t) => self.spans.insert(t.span.start, token_type::TYPE),
            Type::Generic(t) => self.spans.insert(t.span.start, token_type::TYPE),
            // Both segments sit in a type position, so both take the class the
            // position gives them. The head is not knowably a namespace —
            // `ns::Value` and `Self::Item` parse to the same node — and the
            // symbol map refines it where it resolves one; without that, `type`
            // beats the `enumMember` the `::` heuristic would land on.
            Type::NamespacedGeneric(t) => {
                self.spans.insert(t.span.start, token_type::TYPE);
                self.spans.insert(t.name_span.start, token_type::TYPE);
            }
            Type::TypePackSpread(_, span) => self.spans.insert(span.start, token_type::TYPE),
            Type::Function(f) => self.mark_effect_names(&f.effect_ids),
            Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::Infer(_)
            | Type::Error(_) => {}
        }
        ast::walk_type(self, ty);
    }
}

// --- Lexer token classification ---

/// The class of one token, or `None` where nothing should be coloured.
fn classify_token(
    tokens: &[Token],
    index: usize,
    ast_spans: &AstSpans,
    sem_classes: Option<&IndexMap<usize, (u32, u32)>>,
) -> Option<(u32, u32)> {
    let token = &tokens[index];
    if token.kind == TokenKind::Eof || token.span.start == token.span.end {
        return None;
    }

    let class = match token.kind.keyword_category() {
        // Keywords, coloured by their editorial category rather than by being
        // keyword *tokens*: `true` / `false` / `null` are constants and
        // `matches` is an operator.
        Some(category) => classify_keyword(category),

        None => match &token.kind {
            // `self` is the one contextual keyword the language reserves —
            // `Wado.g4`'s `identifier` rule accepts every other one as a name,
            // but not this — so it needs no AST position to be recognised, and
            // the registry files it under `Constant`. Without this it colours
            // as the parameter binding it resolves to.
            TokenKind::Ident(name) if name == "self" => CONSTANT,

            // Identifiers. A name the AST settles outright — a contextual
            // keyword, a field name — outranks whatever else would classify
            // it; everything else prefers the resolved symbol classification
            // (precomputed in `sem_classes`, keyed by byte start) and falls
            // back to the lexer/AST heuristics.
            TokenKind::Ident(_) => ast_spans
                .override_at(token.span.start)
                .or_else(|| sem_classes.and_then(|classes| classes.get(&token.span.start).copied()))
                .unwrap_or_else(|| classify_ident(tokens, index, ast_spans)),

            // Literals
            TokenKind::NumberLit(_) => (token_type::NUMBER, 0),
            TokenKind::StringLit(_)
            | TokenKind::ByteStringLit(_)
            | TokenKind::TemplateStringLit(_)
            | TokenKind::ByteCharLit(_)
            | TokenKind::CharLit(_) => (token_type::STRING, 0),

            // Operators (the highlightable subset; the registry's
            // `is_highlight_operator` flag excludes punctuation-like tokens).
            k if k.is_highlight_operator() => (token_type::OPERATOR, 0),

            // Punctuation — skip (don't emit semantic tokens for brackets, commas, etc.)
            _ => return None,
        },
    };
    Some(class)
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

/// The class a keyword's editorial category implies. `Constant` and `Operator`
/// exist in the registry precisely because those words must not be coloured as
/// keywords.
fn classify_keyword(category: KeywordCategory) -> (u32, u32) {
    match category {
        KeywordCategory::Control
        | KeywordCategory::StorageType
        | KeywordCategory::StorageModifier
        | KeywordCategory::Other => KEYWORD,
        KeywordCategory::Constant => CONSTANT,
        KeywordCategory::Operator => (token_type::OPERATOR, 0),
    }
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
                // After `.`: method if the call follows, else property. A
                // `::<` turbofish is a call's type arguments — `.collect::<T>()`
                // is as much a method as `.collect()`.
                if follows(tokens, index, 1, &TokenKind::LParen)
                    || (follows(tokens, index, 1, &TokenKind::ColonColon)
                        && follows(tokens, index, 2, &TokenKind::Lt))
                {
                    return (token_type::METHOD, 0);
                }
                return (token_type::PROPERTY, 0);
            }
            TokenKind::ColonColon => {
                // After `::`: could be enum member or static method
                if follows(tokens, index, 1, &TokenKind::LParen) {
                    return (token_type::FUNCTION, 0);
                }
                if follows(tokens, index, 1, &TokenKind::ColonColon) {
                    // `::<` is a turbofish, so this is the callee it applies
                    // to, not the middle of a path like `A::B::C`.
                    if follows(tokens, index, 2, &TokenKind::Lt) {
                        return (token_type::FUNCTION, 0);
                    }
                    return (token_type::TYPE, 0);
                }
                return (token_type::ENUM_MEMBER, 0);
            }
            _ => {}
        }
    }

    // 3. A call follows, directly or past its turbofish: `f(`, `f::<T>(`. A
    // `::` onto a name instead is a path, and step 2 classifies its segments.
    if follows(tokens, index, 1, &TokenKind::LParen)
        || (follows(tokens, index, 1, &TokenKind::ColonColon)
            && follows(tokens, index, 2, &TokenKind::Lt))
    {
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

/// Whether the token `ahead` positions past `index` is of `kind`. `ahead` is 1
/// for the next token; the `::<` turbofish needs 2.
fn follows(tokens: &[Token], index: usize, ahead: usize, kind: &TokenKind) -> bool {
    tokens
        .get(index + ahead)
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
            (token_modifier::DEFAULT_LIBRARY, "defaultLibrary"),
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
        // Asserted without semantics: only the AST walk can classify these, so
        // the symbol map cannot mask a regression.
        let src = "resource R {\n    fn m(&self, count: Wide) -> Tall;\n}\nfn run() {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "Wide"), token_type::TYPE);
        assert_eq!(kind_of(&tokens, src, 1, "Tall"), token_type::TYPE);
    }

    /// Only the AST knows, so this asserts the no-semantics path.
    #[test]
    fn struct_literal_field_names_are_properties() {
        let src = "struct Gen {\n    state: i32,\n}\nfn run() {\n    let g = Gen {\n        state: 1,\n    };\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 5, "state"), token_type::PROPERTY);
    }

    /// Attribute keys are not expressions, so only the `use` item's own walk
    /// reaches them — the nested ones included.
    #[test]
    fn import_attribute_keys_are_properties() {
        let src = "use { f } from \"./m.wado\"\n    with {\n        generator: {\n            module: \"lib:gale\",\n        },\n    };\nfn run() {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 2, "generator"), token_type::PROPERTY);
        assert_eq!(kind_of(&tokens, src, 3, "module"), token_type::PROPERTY);
    }

    #[test]
    fn import_attribute_keys_under_an_array_are_properties() {
        let src = "use { f } from \"./m.wado\"\n    with {\n        options: {\n            rules: [\n                { name: \"expr\" },\n            ],\n        },\n    };\nfn run() {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 4, "name"), token_type::PROPERTY);
    }

    #[test]
    fn a_shorthand_struct_literal_field_is_a_property() {
        let src = "struct Gen {\n    state: i32,\n}\nfn run() {\n    let state = 1;\n    let g = Gen {\n        state,\n    };\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 6, "state"), token_type::PROPERTY);
    }

    #[test]
    fn a_struct_pattern_field_is_a_property() {
        let src = "fn run() {\n    let p = { x: 1, y: 2 };\n    let {\n        x,\n        y: renamed,\n    } = p;\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 3, "x"), token_type::PROPERTY);
        assert_eq!(kind_of(&tokens, src, 4, "y"), token_type::PROPERTY);
    }

    #[test]
    fn a_closure_type_bound_signature_is_types() {
        let src =
            "effect E {\n    fn e();\n}\nfn apply<\n    T: fn(i32) -> f64 with E,\n>(f: T) {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 4, "i32"), token_type::TYPE);
        assert_eq!(kind_of(&tokens, src, 4, "f64"), token_type::TYPE);
        assert_eq!(kind_of(&tokens, src, 4, "E"), token_type::TYPE);
    }

    /// An effect named in a `with` clause reads as the type its declaration
    /// does — on the function's own clause and inside an `fn(…) with E` type.
    #[test]
    fn an_effect_named_in_a_with_clause_is_a_type() {
        let src = "effect E {\n    fn f();\n}\nfn run<\n    T,\n>(\n    body: fn mut() -> T with E,\n) -> T with E {\n    return body();\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 6, "E"), token_type::TYPE);
        assert_eq!(kind_of(&tokens, src, 7, "E"), token_type::TYPE);
    }

    /// A path's turbofish is pinned on the identifier, not on a call, when no
    /// call follows — and its arguments are types there too.
    #[test]
    fn a_path_prefix_turbofish_argument_is_a_type() {
        let src = "variant Opt<V> {\n    None,\n    Some(V),\n}\nfn run() {\n    let b = Opt::<i32>::None;\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 5, "i32"), token_type::TYPE);
    }

    /// `Output` in `Builder<Output = T>` names an associated type.
    #[test]
    fn an_associated_type_binding_is_a_type() {
        let src = "fn f<\n    B: Builder<Output = i32>,\n>() {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "Output"), token_type::TYPE);
    }

    /// A type pack's member sits in a type position and is one — at its
    /// declaration, where the `..` starts the parameter's span, as much as at
    /// its use.
    #[test]
    fn a_type_pack_spread_member_is_a_type() {
        let src = "fn f<\n    ..T,\n>(\n    items: [..T],\n) {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "T"), token_type::TYPE_PARAMETER);
        assert_eq!(kind_of(&tokens, src, 3, "T"), token_type::TYPE);
    }

    /// Both segments of `ns::Value` are in a type position. The head is not
    /// knowably a namespace — `Self::Item` parses the same — so it takes the
    /// class the position gives it rather than the one resolution would.
    #[test]
    fn a_namespaced_type_is_a_type_on_both_segments() {
        let src = "fn f(\n    v: ns::Value,\n) {}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "ns"), token_type::TYPE);
        assert_eq!(kind_of(&tokens, src, 1, "Value"), token_type::TYPE);
    }

    /// `::<` is a turbofish, so the name before it is the callee, not the
    /// middle of a path like `A::B::C`.
    #[test]
    fn a_turbofish_callee_is_a_function_not_a_path_middle() {
        let src = "fn run() {\n    let x = builtin::array_new::<u8>(1);\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "array_new"), token_type::FUNCTION);
    }

    /// An unqualified callee carries its turbofish the same way, with no `.`
    /// or `::` ahead of it to classify it first.
    #[test]
    fn a_turbofish_call_on_a_bare_name_is_a_function() {
        let src = "fn identity<T>(x: T) -> T {\n    return x;\n}\nfn run() {\n    let n = identity::<i32>(1);\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 4, "identity"), token_type::FUNCTION);
    }

    /// A `::` onto a name is a path, not a call: the head stays a plain name.
    #[test]
    fn a_path_head_is_not_a_function() {
        let src = "fn run() {\n    let x = builtin::array_new::<u8>(1);\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "builtin"), token_type::VARIABLE);
    }

    /// The same turbofish after a `.`: type arguments do not make a call a
    /// field read.
    #[test]
    fn a_turbofish_method_call_is_a_method() {
        let src = "fn run() {\n    let d = \"h\".bytes().collect::<List<u8>>();\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "collect"), token_type::METHOD);
    }

    #[test]
    fn test_block_closure_parameter_is_a_parameter() {
        let src =
            "fn run() {}\ntest \"t\" {\n    let g = |count: i32| count;\n    let _ = g(1);\n}\n";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));
        assert_eq!(kind_of(&tokens, src, 2, "count"), token_type::PARAMETER);
    }

    #[test]
    fn struct_field_default_expression_is_walked() {
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

    /// Class (type + modifiers) of the token whose text is `needle` on `line`.
    fn class_of(tokens: &[SemanticToken], src: &str, line: u32, needle: &str) -> (u32, u32) {
        let text = src.split_inclusive('\n').nth(line as usize).expect("line");
        let col = text.find(needle).expect("needle on line") as u32;
        let token = tokens
            .iter()
            .find(|t| t.line == line && t.start_char == col)
            .unwrap_or_else(|| panic!("no token for {needle:?} at {line}:{col}"));
        (token.token_type, token.modifiers)
    }

    /// `true` / `false` / `null` are `KeywordCategory::Constant`: keyword
    /// tokens the registry says must not be coloured as keywords.
    #[test]
    fn language_constants_are_constants_not_keywords() {
        let src = "fn run() {\n    let a = true;\n    let b = false;\n    let c = null;\n}\n";
        let tokens = compute(src, None);
        assert_eq!(class_of(&tokens, src, 1, "true"), CONSTANT);
        assert_eq!(class_of(&tokens, src, 2, "false"), CONSTANT);
        assert_eq!(class_of(&tokens, src, 3, "null"), CONSTANT);
    }

    /// A template literal is one lexer token, so without expansion everything
    /// inside `${…}` is painted string and the code there goes uncoloured.
    #[test]
    fn template_interpolation_is_classified_as_code() {
        let src = "fn run() {\n    let n = 1;\n    let _ = `v=${n + 2}!`;\n}\n";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));
        assert_eq!(kind_of(&tokens, src, 2, "n + 2"), token_type::VARIABLE);
        assert_eq!(kind_of(&tokens, src, 2, "+"), token_type::OPERATOR);
        assert_eq!(kind_of(&tokens, src, 2, "2}"), token_type::NUMBER);
        // The text around the interpolation, `${` included, stays string.
        assert_eq!(kind_of(&tokens, src, 2, "`v=${"), token_type::STRING);
        assert_eq!(kind_of(&tokens, src, 2, "}!`"), token_type::STRING);
    }

    /// The `:spec` tail is metadata, not code: `${x:>8}` must not colour `>`
    /// as an operator, which the expansion would do if it stopped at the `}`.
    #[test]
    fn template_format_specifier_is_muted() {
        let src = "fn run() {\n    let n = 1;\n    let _ = `${n:>8}`;\n}\n";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));
        assert_eq!(kind_of(&tokens, src, 2, "n:>8"), token_type::VARIABLE);
        assert_eq!(kind_of(&tokens, src, 2, ":>8"), token_type::COMMENT);
        assert_eq!(kind_of(&tokens, src, 2, "}`"), token_type::STRING);
    }

    /// The lexer keeps comments out of the token stream, so an interpolation's
    /// comment reaches the classifier only through the fragment's `comments`.
    #[test]
    fn template_interpolation_comment_is_a_comment() {
        let src = "fn run() {\n    let n = 1;\n    let _ = `${ /* why */ n }`;\n}\n";
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));
        assert_eq!(kind_of(&tokens, src, 2, "/* why */"), token_type::COMMENT);
        assert_eq!(kind_of(&tokens, src, 2, "n }`"), token_type::VARIABLE);
    }

    /// `matches` lexes as a keyword but is a binary pattern-test operator.
    #[test]
    fn matches_is_an_operator() {
        let src = "fn run(x: Option<i32>) -> bool {\n    return x matches { Some(_) };\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "matches"), token_type::OPERATOR);
    }

    /// `self` is `KeywordCategory::Constant`, so no occurrence may colour as
    /// the parameter it resolves to — the receiver, the uses, and the ones in
    /// positions no AST node carries a span for, like a `stores[self]` clause.
    #[test]
    fn every_self_is_a_constant() {
        let src = concat!(
            "struct S { n: i32 }\n",
            "impl S {\n",
            "    fn get(&self) -> i32 with stores[self] { return self.n; }\n",
            "}\n",
            "fn run() {}\n",
        );
        let sem = sem_of(src);
        let tokens = compute(src, Some(&sem));
        let line = src.split_inclusive('\n').nth(2).expect("line");
        let columns: Vec<u32> = line
            .match_indices("self")
            .map(|(at, _)| at as u32)
            .collect();
        assert_eq!(columns.len(), 3, "receiver, stores clause, and use site");
        for col in columns {
            let token = tokens
                .iter()
                .find(|t| t.line == 2 && t.start_char == col)
                .unwrap_or_else(|| panic!("no token at 2:{col}"));
            assert_eq!((token.token_type, token.modifiers), CONSTANT);
        }
    }

    /// A `__DATA__` tail is not Wado, and no token carries it. `classify_all`
    /// mutes it; `compute` drops the run because it spans lines, which is what
    /// leaves the editor's embedded-JSON highlighting in place.
    #[test]
    fn data_section_is_muted() {
        let src = "fn run() {}\n__DATA__\n{ \"expect\": 1 }\n";
        let marker = src.find("__DATA__").expect("marker");
        let muted = classify_all(src, None)
            .into_iter()
            .find(|t| t.span.start == marker)
            .expect("the data section is classified");
        assert_eq!(muted.token_type, token_type::COMMENT);
        assert_eq!(muted.span.end, src.len());
        assert!(
            compute(src, None).iter().all(|t| t.line < 1),
            "a multi-line run cannot go on the LSP wire",
        );
    }

    /// `b'0'` lexes as its own token kind, which the literal arm did not
    /// list — so it fell through to punctuation and went uncoloured.
    #[test]
    fn byte_char_literal_is_a_string() {
        let src = "fn is_digit(b: u8) -> bool {\n    return b >= b'0';\n}\n";
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 1, "b'0'"), token_type::STRING);
    }

    /// `task`, `trap`, and `forward` lex as identifiers, so without an AST
    /// span they fall through to the identifier heuristic and colour as
    /// variables.
    #[test]
    fn contextual_keywords_task_trap_and_forward_are_keywords() {
        let src = concat!(
            "effect E {\n",
            "    fn ask() -> i32;\n",
            "}\n",
            "struct H {}\n",
            "impl E for H {\n",
            "    ..trap\n",
            "}\n",
            "struct G {}\n",
            "impl E for G {\n",
            "    ..forward\n",
            "}\n",
            "export async fn run() -> i32 {\n",
            "    task return 1;\n",
            "}\n",
        );
        let tokens = compute(src, None);
        assert_eq!(kind_of(&tokens, src, 5, "trap"), token_type::KEYWORD);
        assert_eq!(kind_of(&tokens, src, 9, "forward"), token_type::KEYWORD);
        assert_eq!(kind_of(&tokens, src, 12, "task"), token_type::KEYWORD);
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
