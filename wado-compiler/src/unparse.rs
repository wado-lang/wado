// Unparser for Wado AST
//
// Converts AST back to canonical source code with comments.

use crate::ast::{
    AssertStmt, AssignExpr, AttrArg, Attribute, BinaryExpr, BinaryOp, Block, BreakStmt,
    BuiltinTypeDecl, CallExpr, CastExpr, ClosureExpr, ComparisonChainExpr, CompoundAssignExpr,
    CompoundAssignOp, Condition, ConditionElement, EnumCase, EnumDecl, Expr, ExprStmt,
    FieldAccessExpr, FlagsDecl, ForOfStmt, ForStmt, Function, FunctionType, GenericParam,
    GlobalDecl, IfExpr, IfStmt, ImplBlock, ImportAttributes, IndexExpr, InterfaceDecl,
    InterfaceMethod, Item, LabeledBlockStmt, LetStmt, Literal, LoopStmt, MatchArm, MatchExpr,
    MethodCallExpr, Module, Newtype, Param, Pattern, ResourceDecl, ReturnStmt, SelfKind,
    StaticMethodCallExpr, Stmt, StoresEntry, StructDecl, StructField, StructLiteralExpr,
    TemplateStringExpr, TestDecl, TraitDecl, TupleLiteralExpr, TupleTypeDecl, Type, UnaryExpr,
    UnaryOp, UseDecl, UseItem, UseItemSimple, VariantCase, VariantDecl, WhileStmt, WorldDecl,
    WorldExport,
};
use crate::comment::{Comment, CommentKind};
use crate::hashmap::IndexSet;
use crate::token::Span;

const MAX_LINE_WIDTH: usize = 120;

fn effective_start_line(attrs: &[Attribute], span_line: usize) -> usize {
    attrs
        .first()
        .map_or(span_line, |attr| attr.span.line.min(span_line))
}

/// Returns true if `ty` is the unit type `()`, which is the default return type
/// and therefore omitted from rendered signatures.
fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Named(n) if n.name == "()")
}

/// The namespace name of a `use name from "..."` import, if this declaration is
/// one (a single `Namespace` item).
fn use_namespace_name(u: &UseDecl) -> Option<&str> {
    match u.items.as_slice() {
        [UseItem::Namespace { name }] => Some(name.as_str()),
        _ => None,
    }
}

/// Whether this is a wildcard import: `use _ from "..."`.
fn use_is_wildcard(u: &UseDecl) -> bool {
    matches!(u.items.as_slice(), [UseItem::Wildcard])
}

/// Whether this declaration carries a non-empty `with { ... }` clause.
fn has_with(u: &UseDecl) -> bool {
    u.attributes
        .as_ref()
        .is_some_and(|attrs| !attrs.entries.is_empty())
}

/// Structural nesting depth of an attribute value: scalars (and empty
/// containers) are 0, a container of scalars is 1, a container holding another
/// container is 2, and so on.
fn attr_value_depth(v: &crate::ast::AttrValue) -> usize {
    use crate::ast::AttrValue;
    match v {
        AttrValue::Array(items) if !items.is_empty() => {
            1 + items.iter().map(attr_value_depth).max().unwrap_or(0)
        }
        AttrValue::Object(obj) if !obj.is_empty() => {
            1 + obj.values().map(attr_value_depth).max().unwrap_or(0)
        }
        _ => 0,
    }
}

/// Whether the `with { ... }` attribute object must be expanded multi-line by
/// the depth rule — i.e. it holds at least one nested container.
fn attrs_force_multiline(u: &UseDecl) -> bool {
    u.attributes
        .as_ref()
        .is_some_and(|attrs| attrs.entries.values().any(|v| attr_value_depth(v) >= 1))
}

/// Append each item separated by `", "` via `emit`.
fn comma_sep_into<I, F>(items: I, output: &mut String, emit: F)
where
    I: IntoIterator,
    F: FnMut(I::Item, &mut String),
{
    comma_sep_with_into(", ", items, output, emit);
}

/// Like `comma_sep_into`, but with a custom separator.
fn comma_sep_with_into<I, F>(separator: &str, items: I, output: &mut String, mut emit: F)
where
    I: IntoIterator,
    F: FnMut(I::Item, &mut String),
{
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            output.push_str(separator);
        }
        emit(item, output);
    }
}

/// Append `open`, comma-separated items, then `close`.
fn delimited_into<I, F>(open: &str, close: &str, items: I, output: &mut String, emit: F)
where
    I: IntoIterator,
    F: FnMut(I::Item, &mut String),
{
    output.push_str(open);
    comma_sep_into(items, output, emit);
    output.push_str(close);
}

/// Append `kw` to `output` if `cond` is true. Free-function counterpart of
/// `Unparser::emit_kw_if`.
fn emit_kw_if_into(cond: bool, kw: &str, output: &mut String) {
    if cond {
        output.push_str(kw);
    }
}

/// Number of blank lines the formatter emits between two source lines.
///
/// This is the formatter's gap-display rule: if the source had no blank
/// line between the two anchors we emit none, exactly one blank line is
/// preserved as one, and any larger gap collapses to two so an arbitrary
/// number of empty lines in the input cannot blow up output size.
fn blank_lines_between(prev_line: usize, next_line: usize) -> usize {
    if next_line <= prev_line + 1 {
        return 0;
    }
    let blank_count = next_line - prev_line - 1;
    match blank_count {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}

fn contains_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticMethodCall(_) => true,
        Expr::Binary(e) => contains_call(&e.left) || contains_call(&e.right),
        Expr::Unary(e) => contains_call(&e.expr),
        Expr::Cast(e) => contains_call(&e.expr),
        Expr::TupleLiteral(e) => e.elements.iter().any(contains_call),
        Expr::StructLiteral(e) => e.fields.iter().any(|f| contains_call(&f.value)),
        Expr::Index(e) => contains_call(&e.expr) || contains_call(&e.index),
        Expr::FieldAccess(e) => contains_call(&e.expr),
        Expr::TryOp(e) => contains_call(&e.expr),
        Expr::Spread(e, _) => contains_call(e),
        _ => false,
    }
}

/// Whether an expression is a non-empty kv/seq literal (array/tuple or struct
/// literal). Such an element is treated as a nested container by the depth
/// rule: it forces the surrounding literal multi-line and sits on its own line.
fn expr_is_container(expr: &Expr) -> bool {
    match expr {
        Expr::TupleLiteral(t) => !t.elements.is_empty(),
        Expr::StructLiteral(s) => !s.fields.is_empty(),
        _ => false,
    }
}

#[derive(Default)]
pub struct Unparser<'a> {
    /// AstId-keyed trivia store, populated by the parser plus the
    /// `populate_trailing` / `populate_inner_tail` post-parse visitors.
    /// Every comment-emitting path reads `trivia.leading_of(id)`,
    /// `trivia.trailing_of(id)`, or `trivia.inner_tail_of(id)`. `None`
    /// means "render without comments" (used by `wado dump`'s
    /// AST-rendering path, where comment fidelity is not required).
    trivia: Option<&'a crate::comment::TriviaMap>,
    output: String,
    indent_level: usize,
    emitted_comments: IndexSet<usize>,
    last_source_line: usize,
}

impl<'a> Unparser<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a parser-populated [`TriviaMap`]. Builder-style so the
    /// formatter pipeline can opt in with
    /// `Unparser::new().with_trivia(&trivia)`, while paths that don't
    /// care about comments (e.g. AST dump) just call `Unparser::new()`.
    pub fn with_trivia(mut self, trivia: &'a crate::comment::TriviaMap) -> Self {
        self.trivia = Some(trivia);
        self
    }

    /// Leading trivia for `id`, or an empty slice when no trivia map is
    /// attached or the id has no recorded leading comments.
    fn leading_of(&self, id: crate::ast::AstId) -> &'a [crate::comment::Comment] {
        self.trivia.map(|t| t.leading_of(id)).unwrap_or(&[])
    }

    /// Trailing trivia for `id`. Same fallback as [`Self::leading_of`].
    fn trailing_of(&self, id: crate::ast::AstId) -> &'a [crate::comment::Comment] {
        self.trivia.map(|t| t.trailing_of(id)).unwrap_or(&[])
    }

    /// Inner-tail trivia for a block `id`. Same fallback as [`Self::leading_of`].
    fn inner_tail_of(&self, id: crate::ast::AstId) -> &'a [crate::comment::Comment] {
        self.trivia.map(|t| t.inner_tail_of(id)).unwrap_or(&[])
    }

    /// Emit any leading block-trivia comments for `id` inline at the
    /// current cursor, each followed by a single space. Used by
    /// delimited-list emit (`emit_inline_arg_leading`,
    /// `emit_multiline_arg_leading`) so `foo(/*x=*/1, /*y=*/2)` round-
    /// trips. `Line` / `DocLine` / `ModuleDoc` comments would force a
    /// newline mid-expression and are skipped here — those kinds are
    /// not legal in inline position anyway.
    fn emit_inline_leading_for(&mut self, id: crate::ast::AstId) {
        // Collect into a local Vec to satisfy the borrow checker — the
        // immutable borrow of `self.trivia` cannot live across the
        // mutable `self.output` / `self.emitted_comments` writes below.
        let comments: Vec<crate::comment::Comment> = self.leading_of(id).to_vec();
        for comment in comments {
            if !matches!(comment.kind, crate::comment::CommentKind::Block) {
                continue;
            }
            if self.emitted_comments.insert(comment.span.start) {
                self.emit_comment(&comment);
                self.output.push(' ');
            }
        }
    }

    /// Emit leading trivia for an AST node that is about to be written
    /// on its own line (e.g. inside a multiline call-args block where
    /// each arg has been pre-indented). Block comments stay inline with
    /// a trailing space — same as [`Self::emit_inline_leading_for`] —
    /// while line / doc / module-doc comments get their own line with
    /// matching indent so the textual position they had in the source
    /// is preserved.
    fn emit_multiline_leading_for(&mut self, id: crate::ast::AstId) {
        let comments: Vec<crate::comment::Comment> = self.leading_of(id).to_vec();
        for comment in comments {
            if !self.emitted_comments.insert(comment.span.start) {
                continue;
            }
            match comment.kind {
                crate::comment::CommentKind::Block => {
                    self.emit_comment(&comment);
                    self.output.push(' ');
                }
                crate::comment::CommentKind::Line
                | crate::comment::CommentKind::DocLine
                | crate::comment::CommentKind::ModuleDoc => {
                    self.emit_comment(&comment);
                    self.output.push('\n');
                    self.write_indent();
                }
            }
        }
    }

    /// Emit blank lines to reach the target line, updating `last_source_line`
    fn emit_blank_lines_to(&mut self, target_line: usize) {
        if self.last_source_line > 0 && target_line > self.last_source_line {
            let blanks = blank_lines_between(self.last_source_line, target_line);
            for _ in 0..blanks {
                self.output.push('\n');
            }
        }
        self.last_source_line = target_line;
    }

    pub fn unparse(mut self, module: &Module) -> String {
        if let Some(shebang) = module.shebang() {
            self.output.push_str(shebang);
            self.output.push('\n');
        }

        self.unparse_module(module);

        if let Some(data) = module.data_section() {
            if !self.output.ends_with('\n') {
                self.output.push('\n');
            }
            self.output.push_str("\n__DATA__\n");
            self.output.push_str(data);
        }

        self.output
    }

    fn unparse_module(&mut self, module: &Module) {
        for attr in module.inner_attributes() {
            self.output.push_str("#![");
            self.output.push_str(&attr.name);
            if !attr.args.is_empty() {
                self.delimited("(", ")", &attr.args, Unparser::unparse_attr_arg);
            }
            self.output.push_str("]\n");
            // Anchor `last_source_line` to the inner attr so blank lines
            // between it and the first item are preserved.
            self.last_source_line = attr.span.end_line();
        }

        for item in &module.items {
            let item_span = get_item_span(item);
            self.unparse_item(item);
            self.last_source_line = item_span.end_line();
        }
    }

    fn unparse_item(&mut self, item: &Item) {
        let id = get_item_id(item);

        let last_comment_was_doc = self.emit_leading_for_check_doc(id);

        // Anchor the leading-blank computation to the first attribute, not to
        // the item's own line — otherwise repeated formatting passes would grow
        // blank lines between doc comments and attrs. A trailing doc comment
        // belongs to the item, so don't insert any blanks above it either.
        if !last_comment_was_doc {
            self.emit_blank_lines_to(get_item_first_line(item));
        }

        match item {
            Item::Use(u) => self.unparse_use(u),
            Item::Function(f) => self.unparse_function(f),
            Item::Struct(s) => self.unparse_struct(s),
            Item::Enum(e) => self.unparse_enum(e),
            Item::Variant(v) => self.unparse_variant(v),
            Item::Flags(f) => self.unparse_flags(f),
            Item::Newtype(t) => self.unparse_newtype(t),
            Item::Impl(i) => self.unparse_impl(i),
            Item::Trait(t) => self.unparse_trait(t),
            Item::Interface(e) => self.unparse_interface(e),
            Item::Resource(r) => self.unparse_resource(r),
            Item::World(w) => self.unparse_world(w),
            Item::Test(t) => self.unparse_test(t),
            Item::Global(g) => self.unparse_global(g),
            Item::TupleTypeDecl(d) => self.unparse_tuple_type_decl(d),
            Item::BuiltinTypeDecl(d) => self.unparse_builtin_type_decl(d),
            // The formatter's fail-fast path produces no Item::Error; emit
            // nothing if one is ever unparsed (e.g. a future partial-AST caller).
            Item::Error(_) => {}
        }

        self.emit_trailing_for(id);
    }

    fn unparse_use(&mut self, u: &UseDecl) {
        self.write_indent();

        if u.is_pub {
            self.output.push_str("pub ");
        }

        // The line is rendered as a `(imports_wrapped, with_multiline)` choice.
        // We try candidates in preference order and keep the first whose every
        // line fits in `MAX_LINE_WIDTH`. The import-item list wraps purely by
        // width (items are flat names), while the `with` clause additionally
        // wraps when its attribute object is nested (the depth rule): a `with`
        // whose value contains another container is always expanded, matching
        // how struct/array literals break.
        let snap = self.snapshot();

        let candidates: &[(bool, bool)] = if !has_with(u) {
            &[(false, false), (true, false)]
        } else if attrs_force_multiline(u) {
            &[(false, true), (true, true)]
        } else {
            &[(false, false), (true, false), (false, true), (true, true)]
        };

        for (i, &(wrap_imports, with_multiline)) in candidates.iter().enumerate() {
            self.rollback(snap);
            if wrap_imports {
                self.emit_use_imports_wrapped(u);
            } else {
                self.emit_use_imports_inline(u);
            }
            self.emit_use_from(u);
            if with_multiline {
                self.emit_use_with_multiline(u);
            } else {
                self.emit_use_with_inline(u);
            }
            self.output.push_str(";\n");

            if i + 1 == candidates.len() || !self.exceeds_width_since(snap) {
                return;
            }
        }
    }

    /// Emit the `use <imports>` portion on a single line (no from/with clause).
    fn emit_use_imports_inline(&mut self, u: &UseDecl) {
        if let Some(name) = use_namespace_name(u) {
            self.output.push_str("use ");
            self.output.push_str(name);
        } else if use_is_wildcard(u) {
            self.output.push_str("use _");
        } else {
            self.output.push_str("use { ");
            self.comma_sep(&u.items, Unparser::unparse_use_item);
            self.output.push_str(" }");
        }
    }

    /// Emit the `use <imports>` portion, wrapping a multi-item list one item
    /// per line. Namespace and wildcard forms have nothing to wrap, so they
    /// stay inline.
    fn emit_use_imports_wrapped(&mut self, u: &UseDecl) {
        if use_namespace_name(u).is_some() || use_is_wildcard(u) {
            self.emit_use_imports_inline(u);
            return;
        }
        self.output.push_str("use {\n");
        self.indent_level += 1;
        for item in &u.items {
            self.write_indent();
            self.unparse_use_item(item);
            self.output.push_str(",\n");
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    /// Emit the ` from "..."` source clause.
    fn emit_use_from(&mut self, u: &UseDecl) {
        self.output.push_str(" from \"");
        self.output.push_str(&u.source);
        self.output.push('"');
    }

    /// Emit the ` with { ... }` attribute clause on the same line (or nothing
    /// when there are no attributes).
    fn emit_use_with_inline(&mut self, u: &UseDecl) {
        if let Some(attrs) = &u.attributes {
            self.unparse_import_attributes(attrs);
        }
    }

    /// Emit the `with { ... }` attribute clause wrapped across multiple lines,
    /// with the `with` keyword on its own indented line.
    fn emit_use_with_multiline(&mut self, u: &UseDecl) {
        let Some(attrs) = &u.attributes else { return };
        if attrs.entries.is_empty() {
            return;
        }
        self.output.push('\n');
        self.indent_level += 1;
        self.write_indent();
        self.output.push_str("with ");
        self.unparse_attr_object_multiline(&attrs.entries);
        self.indent_level -= 1;
    }

    fn unparse_use_item(&mut self, item: &UseItem) {
        match item {
            UseItem::Simple { name, alias, .. } => {
                self.output.push_str(name);
                if let Some(alias) = alias {
                    self.output.push_str(" as ");
                    self.output.push_str(alias);
                }
            }
            UseItem::InterfaceFunctions {
                interface_name,
                functions,
            } => {
                self.output.push_str(interface_name);
                self.output.push_str("::{ ");
                self.comma_sep(functions, Unparser::unparse_use_item_simple);
                self.output.push_str(" }");
            }
            UseItem::Wildcard => {
                self.output.push('_');
            }
            UseItem::Namespace { name } => {
                self.output.push_str(name);
            }
        }
    }

    fn unparse_use_item_simple(&mut self, item: &UseItemSimple) {
        self.output.push_str(&item.name);
        if let Some(alias) = &item.alias {
            self.output.push_str(" as ");
            self.output.push_str(alias);
        }
    }

    fn unparse_import_attributes(&mut self, attrs: &ImportAttributes) {
        if attrs.entries.is_empty() {
            return;
        }
        self.output.push_str(" with { ");
        self.comma_sep(&attrs.entries, |s, (k, v)| {
            s.output.push_str(k);
            s.output.push_str(": ");
            s.unparse_attr_value(v);
        });
        self.output.push_str(" }");
    }

    fn unparse_attr_value(&mut self, v: &crate::ast::AttrValue) {
        match v {
            crate::ast::AttrValue::String(s) => {
                self.output.push('"');
                self.output.push_str(s);
                self.output.push('"');
            }
            crate::ast::AttrValue::Int(n) => {
                self.output.push_str(&n.to_string());
            }
            crate::ast::AttrValue::Float(f) => {
                let s = format!("{f}");
                self.output.push_str(&s);
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    self.output.push_str(".0");
                }
            }
            crate::ast::AttrValue::Bool(b) => {
                self.output.push_str(if *b { "true" } else { "false" });
            }
            crate::ast::AttrValue::Array(items) => {
                self.delimited("[", "]", items, Unparser::unparse_attr_value);
            }
            crate::ast::AttrValue::Object(obj) => {
                self.output.push_str("{ ");
                self.comma_sep(obj, |s, (k, v)| {
                    s.output.push_str(k);
                    s.output.push_str(": ");
                    s.unparse_attr_value(v);
                });
                self.output.push_str(" }");
            }
        }
    }

    /// Emit an attribute value. A container nested inside another container
    /// (depth ≥ 2) is always expanded multi-line; a leaf container (depth 1,
    /// only scalar members) is inline-first and falls back to multi-line only
    /// when it overflows. Scalars are always inline.
    fn unparse_attr_value_wrapped(&mut self, v: &crate::ast::AttrValue) {
        match v {
            crate::ast::AttrValue::Object(obj) if !obj.is_empty() => {
                self.emit_container_value(
                    attr_value_depth(v),
                    |s| s.unparse_attr_value(v),
                    |s| {
                        s.unparse_attr_object_multiline(obj);
                    },
                );
            }
            crate::ast::AttrValue::Array(items) if !items.is_empty() => {
                self.emit_container_value(
                    attr_value_depth(v),
                    |s| s.unparse_attr_value(v),
                    |s| {
                        s.unparse_attr_array_multiline(items);
                    },
                );
            }
            _ => self.unparse_attr_value(v),
        }
    }

    /// Shared container-rendering policy: force multi-line at depth ≥ 2,
    /// otherwise try `inline` and roll back to `multiline` only on overflow.
    fn emit_container_value(
        &mut self,
        depth: usize,
        inline: impl Fn(&mut Self),
        multiline: impl Fn(&mut Self),
    ) {
        if depth >= 2 {
            multiline(self);
            return;
        }
        let snap = self.snapshot();
        inline(self);
        if self.exceeds_width_since(snap) {
            self.rollback(snap);
            multiline(self);
        }
    }

    /// Emit `{` then one `key: value,` per line (recursively wrapping each
    /// value as needed), then a closing `}` on its own indented line.
    fn unparse_attr_object_multiline(
        &mut self,
        obj: &crate::hashmap::IndexMap<String, crate::ast::AttrValue>,
    ) {
        self.output.push_str("{\n");
        self.indent_level += 1;
        for (k, v) in obj {
            self.write_indent();
            self.output.push_str(k);
            self.output.push_str(": ");
            self.unparse_attr_value_wrapped(v);
            self.output.push_str(",\n");
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    /// Emit `[` then one element per line (recursively wrapping each as needed),
    /// then a closing `]` on its own indented line.
    fn unparse_attr_array_multiline(&mut self, items: &[crate::ast::AttrValue]) {
        self.output.push_str("[\n");
        self.indent_level += 1;
        for item in items {
            self.write_indent();
            self.unparse_attr_value_wrapped(item);
            self.output.push_str(",\n");
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push(']');
    }

    /// Emit `(params)` followed by `emit_after` (the rest of the signature line
    /// — return type, `with` clause, etc.), breaking the parameters one-per-line
    /// when the whole signature line would overflow the width budget. The width
    /// check spans `emit_after` too, since a short parameter list can still push
    /// the line over once the return type is appended. On overflow the inline
    /// attempt is rolled back and re-emitted wrapped, the same width-aware
    /// rollback the import list uses.
    fn delimited_params<F: Fn(&mut Self)>(&mut self, params: &[Param], emit_after: F) {
        let snap = self.snapshot();
        self.delimited("(", ")", params, Unparser::unparse_param);
        emit_after(self);
        if params.is_empty() || !self.exceeds_width_since(snap) {
            return;
        }
        self.rollback(snap);
        self.output.push_str("(\n");
        self.indent_level += 1;
        for param in params {
            self.write_indent();
            self.unparse_param(param);
            self.output.push_str(",\n");
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push(')');
        emit_after(self);
    }

    fn unparse_function(&mut self, f: &Function) {
        self.emit_outer_attrs(&f.attrs);
        self.emit_kw_if(f.is_pub, "pub ");
        self.emit_kw_if(f.is_export, "export ");
        self.emit_kw_if(f.is_async, "async ");

        self.output.push_str("fn ");
        self.output.push_str(&f.name);
        self.unparse_generic_params(&f.type_params);
        self.delimited_params(&f.params, |s| {
            if let Some(ret) = &f.return_type
                && !is_unit_type(ret)
            {
                s.output.push_str(" -> ");
                s.unparse_type(ret);
            }
            s.unparse_with_clause(&f.effects, &f.stores);
        });

        if let Some(body) = &f.body {
            self.output.push_str(" {\n");
            self.indent_level += 1;
            self.unparse_block(body);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push_str("}\n");
        } else {
            self.output.push_str(";\n");
        }
    }

    fn unparse_with_clause(&mut self, effects: &[String], stores: &[String]) {
        unparse_with_clause_into(effects, stores, &mut self.output);
    }

    fn unparse_param(&mut self, param: &Param) {
        if let Some(self_form) = self_param_shorthand(param) {
            self.output.push_str(self_form);
            return;
        }
        self.emit_kw_if(param.is_mut, "mut ");
        self.output.push_str(&param.name);
        self.output.push_str(": ");
        self.unparse_type(&param.ty);
        // Default expressions are effect-free (enforced by the analyzer), so
        // they typically have no comments to preserve. We still go through the
        // comment-aware `unparse_expr` to keep behavior consistent with bodies.
        if let Some(default) = &param.default {
            self.output.push_str(" = ");
            self.unparse_expr(default);
        }
    }

    fn unparse_attr_arg(&mut self, arg: &AttrArg) {
        match arg {
            AttrArg::Str(s) => {
                self.output.push('"');
                self.output.push_str(s);
                self.output.push('"');
            }
            AttrArg::Ident(s) | AttrArg::Number(s) => {
                self.output.push_str(s);
            }
            AttrArg::KeyValue(k, v) => {
                self.output.push_str(k);
                self.output.push_str(" = \"");
                self.output.push_str(v);
                self.output.push('"');
            }
            AttrArg::KeyArray(k, vs) => {
                self.output.push_str(k);
                self.output.push_str(" = ");
                self.delimited("[", "]", vs, |s, v| {
                    s.output.push('"');
                    s.output.push_str(v);
                    s.output.push('"');
                });
            }
        }
    }

    fn unparse_attribute(&mut self, attr: &Attribute) {
        self.output.push_str("#[");
        self.output.push_str(&attr.name);
        if !attr.args.is_empty() {
            self.delimited("(", ")", &attr.args, Unparser::unparse_attr_arg);
        }
        self.output.push(']');
    }

    fn unparse_struct(&mut self, s: &StructDecl) {
        self.emit_outer_attrs(&s.attrs);
        self.emit_kw_if(s.is_pub, "pub ");

        self.output.push_str("struct ");
        self.output.push_str(&s.name);
        self.unparse_generic_params(&s.type_params);

        self.with_braced_body(s.span, |this| {
            for field in &s.fields {
                this.emit_member(field.id, field.span, &field.attrs, |this| {
                    this.unparse_struct_field(field);
                });
            }
        });
    }

    fn unparse_struct_field(&mut self, field: &StructField) {
        self.emit_outer_attrs(&field.attrs);
        self.emit_kw_if(field.is_pub, "pub ");
        self.output.push_str(&field.name);
        self.output.push_str(": ");
        self.unparse_type(&field.ty);
        if let Some(default) = &field.default {
            self.output.push_str(" = ");
            self.unparse_expr(default);
        }
        self.output.push(',');
    }

    /// Unparse generic type parameters: `<T, U: Ord>`
    fn unparse_generic_params(&mut self, params: &[crate::ast::GenericParam]) {
        if params.is_empty() {
            return;
        }
        self.delimited("<", ">", params, |s, param| {
            s.emit_kw_if(param.is_effect, "effect ");
            s.emit_kw_if(param.is_pack, "..");
            s.output.push_str(&param.name);
            if !param.bounds.is_empty() {
                s.output.push_str(": ");
                s.comma_sep_with(" + ", &param.bounds, |s, bound| {
                    if let Some(sig) = &bound.fn_signature {
                        // `<F: fn(...)>` / `<F: fn mut(...)>` round-trip.
                        // Use bound-aware fn-type printing so multi-effect
                        // `with` clauses come back out parens-grouped.
                        s.unparse_fn_signature_in_bound(sig);
                    } else {
                        s.output.push_str(&bound.name);
                        if !bound.assoc_types.is_empty() {
                            s.delimited("<", ">", &bound.assoc_types, |s, assoc| {
                                s.output.push_str(&assoc.name);
                                s.output.push_str(" = ");
                                s.unparse_type(&assoc.ty);
                            });
                        }
                    }
                });
            }
            if let Some(default_type) = &param.default {
                s.output.push_str(" = ");
                s.unparse_type(default_type);
            }
        });
    }

    fn unparse_enum(&mut self, e: &EnumDecl) {
        self.emit_outer_attrs(&e.attrs);
        self.emit_kw_if(e.is_pub, "pub ");

        self.output.push_str("enum ");
        self.output.push_str(&e.name);
        self.unparse_generic_params(&e.type_params);

        self.with_braced_body(e.span, |this| {
            for case in &e.cases {
                this.emit_member(case.id, case.span, &case.attrs, |this| {
                    this.unparse_enum_case(case);
                });
            }
        });
    }

    fn unparse_enum_case(&mut self, case: &EnumCase) {
        self.emit_outer_attrs(&case.attrs);
        self.output.push_str(&case.name);
        // Enum cases have no payload (unlike variant cases)
        self.output.push(',');
    }

    fn unparse_variant(&mut self, v: &VariantDecl) {
        self.emit_outer_attrs(&v.attrs);
        self.emit_kw_if(v.is_pub, "pub ");

        self.output.push_str("variant ");
        self.output.push_str(&v.name);
        self.unparse_generic_params(&v.type_params);

        self.with_braced_body(v.span, |this| {
            for case in &v.cases {
                this.emit_member(case.id, case.span, &case.attrs, |this| {
                    this.unparse_variant_case(case);
                });
            }
        });
    }

    fn unparse_variant_case(&mut self, case: &VariantCase) {
        self.emit_outer_attrs(&case.attrs);
        self.output.push_str(&case.name);
        if let Some(payload) = &case.payload {
            self.output.push('(');
            self.unparse_type(payload);
            self.output.push(')');
        }
        self.output.push(',');
    }

    fn unparse_flags(&mut self, f: &crate::ast::FlagsDecl) {
        self.emit_outer_attrs(f.attributes.as_deref().unwrap_or(&[]));
        self.emit_kw_if(f.is_pub, "pub ");

        self.output.push_str("flags ");
        self.output.push_str(&f.name);

        self.with_braced_body(f.span, |this| {
            for flag in &f.flags {
                this.emit_member(flag.id, flag.span, &flag.attrs, |this| {
                    this.emit_outer_attrs(&flag.attrs);
                    this.output.push_str(&flag.name);
                    this.output.push(',');
                });
            }
        });
    }

    fn unparse_tuple_type_decl(&mut self, d: &TupleTypeDecl) {
        self.emit_outer_attrs(&d.attrs);
        self.emit_kw_if(d.is_pub, "pub ");
        self.output.push_str("type [..T];\n");
    }

    fn unparse_builtin_type_decl(&mut self, d: &BuiltinTypeDecl) {
        self.emit_outer_attrs(&d.attrs);
        self.emit_kw_if(d.is_pub, "pub ");
        self.output.push_str("type ");
        self.output.push_str(&d.name);
        self.unparse_generic_params(&d.type_params);
        self.output.push_str(";\n");
    }

    fn unparse_newtype(&mut self, t: &Newtype) {
        self.emit_outer_attrs(&t.attrs);
        self.emit_kw_if(t.is_pub, "pub ");
        self.output.push_str("type ");
        self.output.push_str(&t.name);
        self.unparse_generic_params(&t.type_params);
        self.output.push_str(" = ");
        self.unparse_type(&t.ty);
        self.output.push_str(";\n");
    }

    /// Output an inherent impl type with type param bounds inlined into type args.
    /// E.g.: `impl<T: Ord> List<T>` → `impl List<T: Ord>`
    fn unparse_impl(&mut self, i: &ImplBlock) {
        self.write_indent();
        self.output.push_str("impl");

        // Always emit explicit type params: `impl<T> Foo<T>`, not compact `impl Foo<T>`
        self.unparse_generic_params(&i.type_params);

        if let Some(trait_type) = &i.trait_type {
            self.output.push(' ');
            self.unparse_type(trait_type);
            self.output.push_str(" for ");
        } else {
            self.output.push(' ');
        }
        self.unparse_type(&i.ty);

        if i.is_synthesize_request {
            self.output.push_str(";\n");
            return;
        }

        self.with_braced_body(i.span, |this| {
            for assoc in &i.associated_types {
                this.emit_member(assoc.id, assoc.span, &[], |this| {
                    this.write_indent();
                    this.output.push_str("type ");
                    this.output.push_str(&assoc.name);
                    this.output.push_str(" = ");
                    this.unparse_type(&assoc.ty);
                    this.output.push(';');
                });
            }

            for assoc_const in &i.constants {
                this.emit_member(assoc_const.id, assoc_const.span, &[], |this| {
                    this.write_indent();
                    this.emit_kw_if(assoc_const.is_pub, "pub ");
                    this.output.push_str("const ");
                    this.output.push_str(&assoc_const.name);
                    this.output.push_str(": ");
                    this.unparse_type(&assoc_const.ty);
                    this.output.push_str(" = ");
                    this.unparse_expr(&assoc_const.value);
                    this.output.push(';');
                });
            }

            // Force one blank line between an associated-type / constant
            // block and the first method, regardless of source spacing.
            // After hand-emitting the blank, anchor `last_source_line`
            // at the first method's `effective_start` (= leading-comment
            // line or first-attribute line, falling back to the method
            // line) so the methods loop's per-comment / per-attribute
            // `emit_blank_lines_to` calls don't re-emit blanks on top.
            let has_declarations = !i.associated_types.is_empty() || !i.constants.is_empty();
            if has_declarations && let Some(first) = i.methods.first() {
                this.output.push('\n');
                let first_effective_line = effective_start_line(&first.attrs, first.span.line);
                this.last_source_line = this
                    .leading_of(first.id)
                    .first()
                    .map_or(first_effective_line, |c| c.span.line);
            }

            for (idx, method) in i.methods.iter().enumerate() {
                // Subsequent methods are visually separated by at
                // least one blank line, even if the source had none.
                // The leading-comment helper would emit zero blanks
                // when `blank_lines_between(...) == 0`, so force the
                // gap and advance `last_source_line` to the helper's
                // anchor in the same way as the assoc-method gap above.
                let effective_line = effective_start_line(&method.attrs, method.span.line);
                let effective_start = this
                    .leading_of(method.id)
                    .first()
                    .map_or(effective_line, |c| c.span.line);
                if idx > 0 && blank_lines_between(this.last_source_line, effective_start) == 0 {
                    this.output.push('\n');
                    this.last_source_line = effective_start;
                }
                this.emit_leading_for(method.id);
                this.emit_blank_lines_to(effective_line);
                this.unparse_function(method);
                this.last_source_line = method.span.end_line();
            }

            // Effect-handler rest pattern: `..` opts the impl in to trapping on
            // any operation of the trait/effect that is not implemented above.
            if i.has_rest {
                if !i.methods.is_empty() {
                    this.output.push('\n');
                }
                this.write_indent();
                this.output.push_str("..\n");
            }
        });
    }

    fn unparse_trait(&mut self, t: &TraitDecl) {
        self.emit_outer_attrs(&t.attrs);
        self.emit_kw_if(t.is_pub, "pub ");

        self.output.push_str("trait ");
        self.output.push_str(&t.name);
        self.unparse_generic_params(&t.type_params);

        self.with_braced_body(t.span, |this| {
            for assoc in &t.associated_types {
                this.write_indent();
                this.output.push_str("type ");
                this.output.push_str(&assoc.name);
                if !assoc.bounds.is_empty() {
                    this.output.push_str(": ");
                    this.comma_sep_with(" + ", &assoc.bounds, |this, bound| {
                        this.output.push_str(&bound.name);
                        if !bound.assoc_types.is_empty() {
                            this.delimited("<", ">", &bound.assoc_types, |this, ab| {
                                this.output.push_str(&ab.name);
                                this.output.push_str(" = ");
                                this.unparse_type(&ab.ty);
                            });
                        }
                    });
                }
                this.output.push_str(";\n");
                this.last_source_line = assoc.span.end_line();
            }

            for method in &t.methods {
                this.emit_leading_for(method.id);
                this.emit_blank_lines_to(method.span.line);
                this.unparse_function(method);
                this.last_source_line = method.span.end_line();
            }
        });
    }

    fn unparse_interface(&mut self, e: &InterfaceDecl) {
        self.emit_outer_attrs(&e.attrs);
        self.emit_kw_if(e.is_pub, "pub ");

        self.output.push_str("interface ");
        self.output.push_str(&e.name);

        self.with_braced_body(e.span, |this| {
            for method in &e.methods {
                let effective_line = effective_start_line(&method.attrs, method.span.line);
                this.emit_leading_for(method.id);
                this.emit_blank_lines_to(effective_line);
                this.unparse_interface_method(method);
                this.last_source_line = method.span.end_line();
            }
        });
    }

    fn unparse_interface_method(&mut self, m: &InterfaceMethod) {
        self.emit_outer_attrs(&m.attrs);
        self.emit_kw_if(m.is_async, "async ");

        self.output.push_str("fn ");
        self.output.push_str(&m.name);
        self.delimited_params(&m.params, |s| {
            if let Some(ret) = &m.return_type
                && !is_unit_type(ret)
            {
                s.output.push_str(" -> ");
                s.unparse_type(ret);
            }
        });

        self.output.push_str(";\n");
    }

    fn unparse_resource(&mut self, r: &ResourceDecl) {
        self.emit_outer_attrs(&r.attrs);
        self.emit_kw_if(r.is_pub, "pub ");

        self.output.push_str("resource ");
        self.output.push_str(&r.name);
        self.unparse_generic_params(&r.type_params);

        if r.methods.is_empty() {
            self.output.push_str(";\n");
            return;
        }

        self.with_braced_body(r.span, |this| {
            for method in &r.methods {
                this.emit_leading_for(method.id);
                this.unparse_interface_method(method);
                this.last_source_line = method.span.end_line();
            }
        });
    }

    fn unparse_world(&mut self, w: &WorldDecl) {
        self.emit_outer_attrs(&w.attrs);
        self.emit_kw_if(w.is_pub, "pub ");

        self.output.push_str("world ");
        self.output.push_str(&w.name);

        self.with_braced_body(w.span, |this| {
            for imp in &w.imports {
                this.emit_blank_lines_to(imp.span.line);
                this.write_indent();
                this.output.push_str("import ");
                this.output.push_str(&imp.interface_name);
                this.output.push_str(";\n");
                this.last_source_line = imp.span.end_line();
            }

            for exp in &w.exports {
                match exp {
                    WorldExport::Interface(iface) => {
                        this.emit_blank_lines_to(iface.span.line);
                        this.write_indent();
                        this.output.push_str("export ");
                        this.output.push_str(&iface.interface_name);
                        this.output.push_str(";\n");
                        this.last_source_line = iface.span.end_line();
                    }
                    WorldExport::Function(func) => {
                        this.emit_blank_lines_to(func.span.line);
                        this.write_indent();
                        this.output.push_str("export ");
                        this.emit_kw_if(func.is_async, "async ");
                        this.output.push_str("fn ");
                        this.output.push_str(&func.name);
                        this.delimited_params(&func.params, |s| {
                            if let Some(ret) = &func.return_type
                                && !is_unit_type(ret)
                            {
                                s.output.push_str(" -> ");
                                s.unparse_type(ret);
                            }
                        });
                        this.output.push_str(";\n");
                    }
                }
            }
        });
    }

    fn unparse_test(&mut self, t: &TestDecl) {
        self.emit_outer_attrs(&t.attributes);
        self.output.push_str("test ");
        if let Some(name) = &t.name {
            self.output.push('"');
            self.output.push_str(name);
            self.output.push_str("\" ");
        }
        self.unparse_block_expr(&t.body);
        self.output.push('\n');
    }

    fn unparse_global(&mut self, g: &GlobalDecl) {
        self.emit_outer_attrs(&g.attributes);
        self.emit_kw_if(g.is_pub, "pub ");
        self.output.push_str("global ");
        self.emit_kw_if(g.mutable, "mut ");
        self.output.push_str(&g.name);
        self.output.push_str(": ");
        self.unparse_type(&g.ty);
        self.output.push_str(" = ");
        self.unparse_expr(&g.initializer);
        self.output.push_str(";\n");
    }

    fn unparse_type(&mut self, ty: &Type) {
        unparse_type_into(ty, &mut self.output);
    }

    /// Print a `fn(...)` / `fn mut(...)` closure-type bound. Multi-effect
    /// `with` clauses are emitted parens-grouped so the round-tripped source
    /// re-parses as a single bound (a bare comma would otherwise be eaten by
    /// the surrounding trait-bound or generic-param list).
    fn unparse_fn_signature_in_bound(&mut self, sig: &FunctionType) {
        unparse_fn_signature_in_bound_into(sig, &mut self.output);
    }

    fn unparse_block(&mut self, block: &Block) {
        let saved_line = self.last_source_line;
        self.last_source_line = block.span.line;

        for stmt in &block.stmts {
            let stmt_span = get_stmt_span(stmt);
            let stmt_id = stmt.id();
            self.emit_leading_for(stmt_id);
            self.emit_blank_lines_to(stmt_span.line);
            self.unparse_stmt(stmt);
            self.emit_trailing_for(stmt_id);
            self.last_source_line = stmt_span.end_line();
        }

        // Comments that landed after the last stmt but before the closing brace
        // would otherwise be dropped — flush them here.
        self.emit_inner_tail_for(block.id);

        self.last_source_line = saved_line.max(block.span.end_line());
    }

    fn unparse_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(l) => self.unparse_let(l),
            Stmt::Expr(e) => self.unparse_expr_stmt(e),
            Stmt::Return(r) => self.unparse_return(r),
            Stmt::TaskReturn(tr) => self.unparse_task_return(tr),
            Stmt::If(i) => self.unparse_if_stmt(i),
            Stmt::While(w) => self.unparse_while(w),
            Stmt::For(f) => self.unparse_for(f),
            Stmt::ForOf(f) => self.unparse_for_of(f),
            Stmt::Loop(l) => self.unparse_loop(l),
            Stmt::Match(m) => self.unparse_match_stmt(m),
            Stmt::Break(b) => self.unparse_break(b),
            Stmt::Continue(_) => self.unparse_continue(),
            Stmt::Assert(a) => self.unparse_assert(a),
            Stmt::LabeledBlock(lb) => self.unparse_labeled_block(lb),
            // The formatter is fail-fast on syntax errors, so this placeholder
            // is never reached; emit nothing to keep the match total.
            Stmt::Error(_) => {}
        }
    }

    fn unparse_labeled_block(&mut self, lb: &LabeledBlockStmt) {
        self.write_indent();
        self.output.push_str(&lb.label);
        self.output.push_str(": {\n");

        self.indent_level += 1;
        self.unparse_block(&lb.block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_let(&mut self, l: &LetStmt) {
        self.write_indent();
        self.output.push_str("let ");

        if l.is_reactive {
            self.output.push_str("reactive ");
        }
        if l.is_mut {
            self.output.push_str("mut ");
        }

        self.unparse_pattern(&l.pattern);

        if let Some(ty) = &l.ty {
            self.output.push_str(": ");
            self.unparse_type(ty);
        }

        if let Some(ref v) = l.value {
            self.output.push_str(" = ");
            self.unparse_expr(v);
        }
        self.output.push_str(";\n");
    }

    fn unparse_expr_stmt(&mut self, e: &ExprStmt) {
        self.write_indent();
        self.unparse_expr(&e.expr);
        self.output.push_str(";\n");
    }

    fn unparse_return(&mut self, r: &ReturnStmt) {
        self.write_indent();
        self.output.push_str("return");
        if let Some(value) = &r.value {
            self.output.push(' ');
            self.unparse_expr(value);
        }
        self.output.push_str(";\n");
    }

    fn unparse_task_return(&mut self, tr: &crate::ast::TaskReturnStmt) {
        self.write_indent();
        self.output.push_str("task return ");
        self.unparse_expr(&tr.value);
        self.output.push_str(";\n");
    }

    fn unparse_if_stmt(&mut self, i: &IfStmt) {
        self.write_indent();
        self.output.push_str("if ");
        self.unparse_condition(&i.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&i.then_block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push('}');

        if let Some(else_block) = &i.else_block {
            // Check if this is an `else if` (else block contains only an if statement)
            if else_block.stmts.len() == 1
                && let Stmt::If(nested_if) = &else_block.stmts[0]
            {
                self.output.push_str(" else ");
                self.unparse_if_stmt_continuation(nested_if);
                return;
            }
            self.output.push_str(" else {\n");
            self.indent_level += 1;
            self.unparse_block(else_block);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push('}');
        }

        self.output.push('\n');
    }

    /// Unparse an if statement continuation (for else-if chains).
    /// This skips the initial indent and final newline since they're handled by the parent.
    fn unparse_if_stmt_continuation(&mut self, i: &IfStmt) {
        self.output.push_str("if ");
        self.unparse_condition(&i.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&i.then_block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push('}');

        if let Some(else_block) = &i.else_block {
            // Check if this is an `else if` (else block contains only an if statement)
            if else_block.stmts.len() == 1
                && let Stmt::If(nested_if) = &else_block.stmts[0]
            {
                self.output.push_str(" else ");
                self.unparse_if_stmt_continuation(nested_if);
                return;
            }
            self.output.push_str(" else {\n");
            self.indent_level += 1;
            self.unparse_block(else_block);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push('}');
        }

        self.output.push('\n');
    }

    fn unparse_condition(&mut self, cond: &Condition) {
        match cond {
            Condition::Expr(expr) => {
                self.unparse_expr(expr);
            }
            Condition::LetChain { elements, .. } => {
                self.comma_sep_with(" && ", elements, |s, elem| match elem {
                    ConditionElement::Let { pattern, expr, .. } => {
                        s.output.push_str("let ");
                        s.unparse_pattern(pattern);
                        s.output.push_str(" = ");
                        s.unparse_expr(expr);
                    }
                    ConditionElement::Expr(expr) => {
                        // In a let-chain, elements are joined by `&&`. If this
                        // element is itself a `&&`/`||` expression, we wrap it
                        // in parens so re-parsing preserves the chain shape:
                        // `let PAT = E && (a && b)` would otherwise re-parse
                        // as three chain elements instead of two.
                        let needs_parens = matches!(
                            expr,
                            Expr::Binary(b) if matches!(b.op, BinaryOp::And | BinaryOp::Or)
                        );
                        s.with_parens_if(needs_parens, |s| s.unparse_expr(expr));
                    }
                });
            }
        }
    }

    fn unparse_while(&mut self, w: &WhileStmt) {
        self.write_indent();
        self.output.push_str("while ");
        self.unparse_condition(&w.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&w.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_for(&mut self, f: &ForStmt) {
        self.write_indent();
        self.output.push_str("for ");

        if let Some(init) = &f.init {
            self.unparse_for_init(init);
        }
        self.output.push_str("; ");

        if let Some(cond) = &f.condition {
            self.unparse_condition(cond);
        }
        self.output.push_str("; ");

        if let Some(update) = &f.update {
            self.unparse_expr(update);
        }

        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&f.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_for_of(&mut self, f: &ForOfStmt) {
        self.write_indent();
        self.output.push_str("for let ");

        if f.is_mut {
            self.output.push_str("mut ");
        }

        self.unparse_pattern(&f.binding);
        self.output.push_str(" of ");
        self.unparse_expr(&f.iterable);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&f.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_for_init(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(l) => {
                self.output.push_str("let ");
                if l.is_mut {
                    self.output.push_str("mut ");
                }
                self.unparse_pattern(&l.pattern);
                if let Some(ty) = &l.ty {
                    self.output.push_str(": ");
                    self.unparse_type(ty);
                }
                if let Some(ref v) = l.value {
                    self.output.push_str(" = ");
                    self.unparse_expr(v);
                }
            }
            Stmt::Expr(e) => {
                self.unparse_expr(&e.expr);
            }
            _ => {}
        }
    }

    fn unparse_loop(&mut self, l: &LoopStmt) {
        self.write_indent();
        self.output.push_str("loop {\n");

        self.indent_level += 1;
        self.unparse_block(&l.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_match_stmt(&mut self, m: &MatchExpr) {
        self.write_indent();
        self.unparse_match_multiline(m);
        self.output.push('\n');
    }

    fn unparse_break(&mut self, b: &BreakStmt) {
        self.write_indent();
        self.output.push_str("break");
        if let Some(label) = &b.label {
            self.output.push(' ');
            self.output.push_str(label);
            if let Some(value) = &b.value {
                self.output.push_str(": ");
                self.unparse_expr(value);
            }
        }
        self.output.push_str(";\n");
    }

    fn unparse_continue(&mut self) {
        self.write_indent();
        self.output.push_str("continue;\n");
    }

    fn unparse_assert(&mut self, a: &AssertStmt) {
        self.write_indent();
        self.output.push_str("assert ");
        self.unparse_expr(&a.condition);
        if let Some(msg) = &a.message {
            self.output.push_str(", ");
            self.unparse_expr(msg);
        }
        self.output.push_str(";\n");
    }

    fn unparse_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(i) => {
                self.output.push_str(&i.name);
                if !i.type_args.is_empty() {
                    self.output.push_str("::<");
                    for (idx, ty) in i.type_args.iter().enumerate() {
                        if idx > 0 {
                            self.output.push_str(", ");
                        }
                        self.unparse_type(ty);
                    }
                    self.output.push('>');
                }
            }
            Expr::Literal(l) => self.unparse_literal(&l.value),
            Expr::Binary(b) => self.unparse_binary(b),
            Expr::Unary(u) => self.unparse_unary(u),
            Expr::Assign(a) => self.unparse_assign(a),
            Expr::CompoundAssign(ca) => self.unparse_compound_assign(ca),
            Expr::ComparisonChain(chain) => self.unparse_comparison_chain(chain),
            Expr::Call(c) => self.unparse_call(c),
            Expr::MethodCall(m) => self.unparse_method_call(m),
            Expr::StaticMethodCall(s) => self.unparse_static_method_call(s),
            Expr::FieldAccess(f) => self.unparse_field_access(f),
            Expr::Index(i) => self.unparse_index(i),
            Expr::Block(b) => self.unparse_block_expr(b),
            Expr::If(i) => self.unparse_if_expr(i),
            Expr::Match(m) => self.unparse_match(m),
            Expr::Matches(m) => self.unparse_matches(m),
            Expr::Closure(c) => self.unparse_closure(c),
            Expr::TemplateString(t) => self.unparse_template_string(t),
            Expr::Cast(c) => self.unparse_cast(c),
            Expr::StructLiteral(s) => self.unparse_struct_literal(s),
            Expr::TupleLiteral(t) => self.unparse_tuple_literal(t),
            Expr::LabeledBlock(lb) => self.unparse_labeled_block_expr(lb),
            Expr::TryOp(qm) => {
                self.unparse_expr(&qm.expr);
                self.output.push('?');
            }
            Expr::Spread(inner, _) => {
                self.output.push_str("..");
                self.unparse_expr(inner);
            }
            Expr::Range(range) => {
                self.unparse_expr(&range.start);
                match range.kind {
                    crate::ast::RangeKind::Exclusive => self.output.push_str("..<"),
                    crate::ast::RangeKind::Inclusive => self.output.push_str("..="),
                }
                self.unparse_expr(&range.end);
            }
            Expr::WithHandler(w) => self.unparse_with_handler(w),
            Expr::Resume(r) => {
                self.output.push_str("resume ");
                self.unparse_expr(&r.value);
            }
            // The formatter is fail-fast on syntax errors, so this placeholder
            // is never reached; emit nothing to keep the match total.
            Expr::Error(_) => {}
        }
    }

    fn unparse_with_handler(&mut self, w: &crate::ast::WithHandlerExpr) {
        self.output.push_str("with ");
        self.comma_sep(&w.handlers, |s, binding| {
            if let Some(effect) = &binding.effect {
                s.unparse_type(effect);
                s.output.push_str(" => ");
            }
            s.unparse_expr(&binding.handler);
        });
        self.output.push_str(" do ");
        self.unparse_block_expr(&w.body);
    }

    fn unparse_matches(&mut self, m: &crate::ast::MatchesExpr) {
        self.unparse_expr(&m.expr);
        self.output.push_str(" matches { ");
        self.unparse_pattern(&m.pattern);
        if let Some(guard) = &m.guard {
            self.output.push_str(" && ");
            self.unparse_expr(guard);
        }
        self.output.push_str(" }");
    }

    fn unparse_labeled_block_expr(&mut self, lb: &crate::ast::LabeledBlockExpr) {
        self.output.push_str(&lb.label);
        self.output.push_str(": {\n");
        self.indent_level += 1;
        for stmt in &lb.block.stmts {
            self.unparse_stmt(stmt);
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    fn unparse_tuple_literal(&mut self, tuple_lit: &TupleLiteralExpr) {
        let elements = &tuple_lit.elements;
        if elements.is_empty() {
            self.output.push_str("[]");
            return;
        }

        // Inline `[a, b, c]` only when the array is flat (no nested container),
        // holds at most one call-bearing element, and fits the width. A nested
        // container (depth rule) or a second call forces the multi-line form so
        // complex / deeply-structured arrays stay readable.
        let has_container = elements.iter().any(expr_is_container);
        let call_elems = elements.iter().filter(|e| contains_call(e)).count();
        if !has_container && call_elems <= 1 {
            let snap = self.snapshot();
            self.delimited("[", "]", elements, Unparser::unparse_expr);
            if !self.output[snap..].contains('\n') && !self.exceeds_width_since(snap) {
                return;
            }
            self.rollback(snap);
        }

        self.emit_fill_bracketed(elements);
    }

    /// Emit elements in `[\n … \n]` block form, packing as many per line as fit
    /// within `MAX_LINE_WIDTH`, while keeping at most one call-bearing element
    /// per line and placing each nested container on its own line. The last
    /// element carries a trailing comma.
    fn emit_fill_bracketed(&mut self, elements: &[Expr]) {
        self.output.push_str("[\n");
        self.indent_level += 1;
        self.write_indent();

        let mut line_has_call = false;
        let mut prev_container = false;
        for (i, elem) in elements.iter().enumerate() {
            let is_container = expr_is_container(elem);
            let elem_call = contains_call(elem);

            if i == 0 {
                self.unparse_expr(elem);
                line_has_call = elem_call;
                prev_container = is_container;
                continue;
            }

            // Containers sit alone on their own line, and a line carries at most
            // one call-bearing element.
            if is_container || prev_container || (elem_call && line_has_call) {
                self.output.push_str(",\n");
                self.write_indent();
                self.unparse_expr(elem);
                line_has_call = elem_call;
                prev_container = is_container;
                continue;
            }

            // Pack onto the current line; fall back to a new line on overflow.
            let snap = self.snapshot();
            self.output.push_str(", ");
            self.unparse_expr(elem);
            if self.exceeds_width_since(snap) {
                self.rollback(snap);
                self.output.push_str(",\n");
                self.write_indent();
                self.unparse_expr(elem);
                line_has_call = elem_call;
            } else {
                line_has_call |= elem_call;
            }
            prev_container = is_container;
        }

        self.output.push_str(",\n");
        self.indent_level -= 1;
        self.write_indent();
        self.output.push(']');
    }

    fn unparse_literal(&mut self, lit: &Literal) {
        match lit {
            Literal::Number(repr) => self.output.push_str(repr),
            Literal::String(raw) => {
                self.output.push('"');
                self.output.push_str(raw);
                self.output.push('"');
            }
            Literal::Char(raw) => {
                self.output.push('\'');
                self.output.push_str(raw);
                self.output.push('\'');
            }
            Literal::Bool(b) => self.output.push_str(if *b { "true" } else { "false" }),
            Literal::Null => self.output.push_str("null"),
            Literal::Unit => self.output.push_str("()"),
            Literal::LocationFile => self.output.push_str("#file"),
            Literal::LocationLine => self.output.push_str("#line"),
            Literal::LocationFunction => self.output.push_str("#function"),
            Literal::DataSection => self.output.push_str("#data"),
            Literal::IncludeStr(path) => {
                self.output.push_str("#include_str(\"");
                self.output.push_str(path);
                self.output.push_str("\")");
            }
            Literal::IncludeBytes(path) => {
                self.output.push_str("#include_bytes(\"");
                self.output.push_str(path);
                self.output.push_str("\")");
            }
        }
    }

    fn unparse_binary(&mut self, b: &BinaryExpr) {
        if matches!(b.op, BinaryOp::And | BinaryOp::Or) {
            let snap = self.snapshot();
            self.unparse_binary_inline(b);
            if !self.output[snap..].contains('\n') && !self.exceeds_width_since(snap) {
                return;
            }
            self.rollback(snap);
            self.unparse_logical_chain_multiline(b);
            return;
        }
        self.unparse_binary_inline(b);
    }

    fn unparse_binary_inline(&mut self, b: &BinaryExpr) {
        self.with_parens_if(needs_parens(&b.left, b.op, true), |s| {
            s.unparse_expr(&b.left);
        });

        self.output.push(' ');
        self.output.push_str(binary_op_str(b.op));
        self.output.push(' ');

        self.with_parens_if(needs_parens(&b.right, b.op, false), |s| {
            s.unparse_expr(&b.right);
        });
    }

    fn unparse_logical_chain_multiline(&mut self, b: &BinaryExpr) {
        let op_str = binary_op_str(b.op);
        let parts = collect_logical_chain_binary(b);

        self.with_parens_if(needs_parens(parts[0], b.op, true), |s| {
            s.unparse_expr(parts[0]);
        });

        for part in &parts[1..] {
            self.output.push('\n');
            self.indent_level += 1;
            self.write_indent();
            self.indent_level -= 1;
            self.output.push_str(op_str);
            self.output.push(' ');
            self.with_parens_if(needs_parens(part, b.op, false), |s| s.unparse_expr(part));
        }
    }

    fn unparse_unary(&mut self, u: &UnaryExpr) {
        self.output.push_str(unary_op_str(u.op));

        // Space between consecutive same operators: "- -5" not "--5", "& &x" not "&&x"
        let needs_space = matches!(
            (&u.op, &u.expr),
            (UnaryOp::Neg, Expr::Unary(inner)) if inner.op == UnaryOp::Neg
        ) || matches!(
            (&u.op, &u.expr),
            (UnaryOp::Ref, Expr::Unary(inner)) if inner.op == UnaryOp::Ref
        );
        if needs_space {
            self.output.push(' ');
        }

        let needs_parens = matches!(
            &u.expr,
            Expr::Binary(_)
                | Expr::Assign(_)
                | Expr::CompoundAssign(_)
                | Expr::ComparisonChain(_)
                | Expr::Cast(_)
        );
        self.with_parens_if(needs_parens, |s| s.unparse_expr(&u.expr));
    }

    fn unparse_assign(&mut self, a: &AssignExpr) {
        self.unparse_expr(&a.target);
        self.output.push_str(" = ");
        self.unparse_expr(&a.value);
    }

    fn unparse_compound_assign(&mut self, ca: &CompoundAssignExpr) {
        self.unparse_expr(&ca.target);
        self.output.push(' ');
        self.output.push_str(compound_op_str(ca.op));
        self.output.push(' ');
        self.unparse_expr(&ca.value);
    }

    fn unparse_comparison_chain(&mut self, chain: &ComparisonChainExpr) {
        // `x as T < y` would re-parse as `x as T<y>` (generic), so cast scrutinees
        // require parens when the first comparison is `<`.
        let first_needs_parens = matches!(&chain.first, Expr::Cast(_))
            && chain
                .comparisons
                .first()
                .is_some_and(|c| c.op == BinaryOp::Lt);
        self.with_parens_if(first_needs_parens, |s| s.unparse_expr(&chain.first));
        for cmp in &chain.comparisons {
            self.output.push(' ');
            self.output.push_str(binary_op_str(cmp.op));
            self.output.push(' ');
            self.unparse_expr(&cmp.right);
        }
    }

    fn unparse_call(&mut self, c: &CallExpr) {
        // Parenthesize callee shapes that would re-parse differently
        // without parens. The rule of thumb: anything whose precedence
        // sits above the postfix level (call/index/field/method), plus a
        // couple of postfix-level shapes that are still syntactically
        // ambiguous with call syntax.
        //
        // - `FieldAccess`: `self.f(args)` would re-parse as a method call.
        // - `Closure`: a closure with an expression body greedily
        //   consumes the rest of the line, so `|x| x + 1(41)` re-parses
        //   as `|x| (x + 1(41))` rather than calling the closure.
        // - `Cast`: `f as T(args)` does not re-parse — `T(args)` is not a
        //   valid type and the trailing `(` becomes a stray token.
        // - `Unary` / `Binary` / `Assign` / `CompoundAssign` /
        //   `ComparisonChain` / `Range`: all live above postfix, so the
        //   call `(args)` would bind tighter on re-parse.
        //
        // `If` / `Match` / `Block` / `LabeledBlock` / `WithHandler` are
        // explicitly excluded: they end with `}` which terminates the
        // expression cleanly before the call's `(`.
        let needs_parens = matches!(
            &c.callee,
            Expr::FieldAccess(_)
                | Expr::Closure(_)
                | Expr::Cast(_)
                | Expr::Unary(_)
                | Expr::Binary(_)
                | Expr::Assign(_)
                | Expr::CompoundAssign(_)
                | Expr::ComparisonChain(_)
                | Expr::Range(_)
        );
        self.with_parens_if(needs_parens, |s| s.unparse_expr(&c.callee));
        self.unparse_turbofish(&c.type_args);
        self.unparse_call_args(&c.args, c.has_trailing_comma);
    }

    fn unparse_method_call(&mut self, m: &MethodCallExpr) {
        // Expressions with lower operator precedence than `.` need parentheses
        // when used as a method receiver, otherwise the formatter produces
        // semantically different code:
        //   `-128.to_string()`    → parsed as `-(128.to_string())`  (wrong)
        //   `127 as i8.to_string()` → parsed as `127 as (i8.to_string())` (wrong)
        let needs_parens = matches!(
            &m.receiver,
            Expr::Unary(_)
                | Expr::Binary(_)
                | Expr::Cast(_)
                | Expr::Assign(_)
                | Expr::CompoundAssign(_)
                | Expr::Range(_)
        );
        self.with_parens_if(needs_parens, |s| s.unparse_expr(&m.receiver));
        self.output.push('.');
        self.output.push_str(&m.method);
        self.unparse_turbofish(&m.type_args);
        self.unparse_call_args(&m.args, m.has_trailing_comma);
    }

    fn unparse_static_method_call(&mut self, s: &StaticMethodCallExpr) {
        // For generic types, use turbofish syntax: Name::<Args>
        match &s.target_type {
            Type::Generic(g) => {
                self.output.push_str(&g.name);
                self.delimited("::<", ">", &g.args, Unparser::unparse_type);
            }
            _ => self.unparse_type(&s.target_type),
        }
        self.output.push_str("::");
        self.output.push_str(&s.method);
        self.unparse_turbofish(&s.type_args);
        self.unparse_call_args(&s.args, s.has_trailing_comma);
    }

    /// Emit `::<T1, T2, ...>` turbofish; emits nothing if `args` is empty.
    fn unparse_turbofish(&mut self, args: &[Type]) {
        if args.is_empty() {
            return;
        }
        self.delimited("::<", ">", args, Unparser::unparse_type);
    }

    fn unparse_call_args(&mut self, args: &[Expr], has_trailing_comma: bool) {
        // Multiline-with-trailing-comma is requested explicitly by the source;
        // in that case we skip the single-line attempt entirely.
        if !has_trailing_comma || args.is_empty() {
            let snap = self.snapshot();
            self.emit_inline_call_args(args);
            if !self.exceeds_width_since(snap) {
                return;
            }
            self.rollback(snap);
        }
        self.emit_multiline_call_args(args);
    }

    /// Single-line `(arg1, arg2, ...)` form. Each argument's
    /// AstId-keyed leading block trivia is emitted inline immediately
    /// before its expression so `foo(/*x=*/1, /*y=*/2)` round-trips.
    fn emit_inline_call_args(&mut self, args: &[Expr]) {
        self.output.push('(');
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.emit_inline_leading_for(arg.id());
            self.unparse_expr(arg);
        }
        self.output.push(')');
    }

    /// Emit `(arg1,\n arg2,\n ...)` with a trailing comma at the current indent.
    /// Same comment-attachment rule as the single-line variant: leading
    /// block trivia for each arg's `AstId` is emitted on the wrapped line
    /// before the arg expression.
    fn emit_multiline_call_args(&mut self, args: &[Expr]) {
        self.output.push_str("(\n");
        self.indent_level += 1;
        for arg in args {
            self.write_indent();
            // Multiline form: line / doc comments must stay on their
            // own line + indent (block comments stay inline with a
            // trailing space). The single-line `inline_leading_for`
            // would silently drop them.
            self.emit_multiline_leading_for(arg.id());
            self.unparse_expr(arg);
            self.output.push_str(",\n");
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push(')');
    }

    fn unparse_field_access(&mut self, f: &FieldAccessExpr) {
        self.unparse_postfix_base(&f.expr);
        self.output.push('.');
        self.output.push_str(&f.field);
    }

    fn unparse_index(&mut self, i: &IndexExpr) {
        self.unparse_postfix_base(&i.expr);
        self.output.push('[');
        self.unparse_expr(&i.index);
        self.output.push(']');
    }

    /// Emit a postfix base expression, wrapping in parens if needed.
    /// Prefix unary ops (*, -, !, &, ~) bind looser than postfix ops ([], ., ()),
    /// so `(*p)[i]` must keep parens — `*p[i]` would mean `*(p[i])`.
    fn unparse_postfix_base(&mut self, expr: &Expr) {
        self.with_parens_if(matches!(expr, Expr::Unary(_)), |s| s.unparse_expr(expr));
    }

    fn unparse_block_expr(&mut self, b: &Block) {
        self.output.push_str("{\n");
        self.indent_level += 1;
        self.unparse_block(b);
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    fn unparse_if_expr(&mut self, i: &IfExpr) {
        if self.try_inline_if_expr(i) {
            return;
        }
        self.unparse_if_expr_multiline(i);
    }

    /// Try to format an if expression on a single line.
    /// Returns true if successful, false if it should fall back to multiline.
    fn try_inline_if_expr(&mut self, i: &IfExpr) -> bool {
        // Only eligible when: plain condition, else exists, both arms are single expressions,
        // and neither expression is a compound construct
        if matches!(i.condition, Condition::LetChain { .. }) {
            return false;
        }
        let Some(else_block) = &i.else_block else {
            return false;
        };
        // No else-if chains
        if else_block.stmts.len() == 1
            && matches!(
                &else_block.stmts[0],
                Stmt::Expr(ExprStmt {
                    expr: Expr::If(_),
                    ..
                })
            )
        {
            return false;
        }
        let Some(then_expr) = block_single_expr(&i.then_block) else {
            return false;
        };
        let Some(else_expr) = block_single_expr(else_block) else {
            return false;
        };
        if !is_inline_safe_expr(then_expr) || !is_inline_safe_expr(else_expr) {
            return false;
        }

        let snap = self.snapshot();
        self.output.push_str("if ");
        self.unparse_condition(&i.condition);
        self.output.push_str(" { ");
        self.unparse_expr(then_expr);
        self.output.push_str(" } else { ");
        self.unparse_expr(else_expr);
        self.output.push_str(" }");

        if self.output[snap..].contains('\n') || self.exceeds_width_since(snap) {
            self.rollback(snap);
            return false;
        }
        true
    }

    fn unparse_if_expr_multiline(&mut self, i: &IfExpr) {
        self.output.push_str("if ");
        self.unparse_condition(&i.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&i.then_block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push('}');

        if let Some(else_block) = &i.else_block {
            // Render `else { if ... }` as `else if ...` and stay on the multiline
            // path so the whole chain shares one layout decision.
            if else_block.stmts.len() == 1
                && let Stmt::Expr(ExprStmt {
                    expr: Expr::If(nested_if),
                    ..
                }) = &else_block.stmts[0]
            {
                self.output.push_str(" else ");
                self.unparse_if_expr_multiline(nested_if);
                return;
            }
            self.output.push_str(" else {\n");
            self.indent_level += 1;
            self.unparse_block(else_block);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push('}');
        }
    }

    fn unparse_match(&mut self, m: &MatchExpr) {
        if self.try_inline_match(m) {
            return;
        }
        self.unparse_match_multiline(m);
    }

    /// Try to format a match expression on a single line.
    fn try_inline_match(&mut self, m: &MatchExpr) -> bool {
        // Match expressions with 2 or more arms are always formatted multiline
        if m.arms.len() >= 2 {
            return false;
        }
        // All arms must have inline-safe bodies, and no comments inside the match body
        if m.arms.iter().any(|arm| !is_inline_safe_expr(&arm.body)) {
            return false;
        }
        if self.subtree_has_trivia(m) {
            return false;
        }

        let snap = self.snapshot();
        self.output.push_str("match ");
        self.unparse_expr(&m.expr);
        self.output.push_str(" { ");
        self.comma_sep(&m.arms, Unparser::emit_match_arm_body);
        self.output.push_str(" }");

        if self.output[snap..].contains('\n') || self.exceeds_width_since(snap) {
            self.rollback(snap);
            return false;
        }
        true
    }

    /// Emit `pattern [&& guard] => body` (without leading indent or trailing comma).
    fn emit_match_arm_body(&mut self, arm: &MatchArm) {
        self.unparse_pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            self.output.push_str(" && ");
            self.unparse_expr(guard);
        }
        self.output.push_str(" => ");
        self.unparse_expr(&arm.body);
    }

    fn unparse_match_multiline(&mut self, m: &MatchExpr) {
        self.output.push_str("match ");
        self.unparse_expr(&m.expr);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        let saved_line = self.last_source_line;
        // Anchor blank-line tracking to the scrutinee's last line, not the
        // `match` keyword's line: a multi-line scrutinee otherwise leaves
        // `blank_lines_between` thinking the lines occupied by the scrutinee
        // were blank source lines, and conjures spurious blanks before arm 0.
        self.last_source_line = m.expr.span().end_line();

        for arm in &m.arms {
            self.emit_leading_for(arm.id);
            self.emit_blank_lines_to(arm.span.line);
            self.unparse_match_arm(arm);
            self.emit_trailing_for_inline(arm.id);
            self.output.push('\n');
            self.last_source_line = arm.span.end_line();
        }
        self.indent_level -= 1;

        self.last_source_line = saved_line.max(m.span.end_line());
        self.write_indent();
        self.output.push('}');
    }

    fn unparse_match_arm(&mut self, arm: &MatchArm) {
        self.write_indent();
        self.emit_match_arm_body(arm);
        self.output.push(',');
    }

    fn unparse_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Ident { name, .. } => self.output.push_str(name),
            Pattern::MutIdent { name, .. } => {
                self.output.push_str("mut ");
                self.output.push_str(name);
            }
            Pattern::Literal(lit) => self.unparse_literal(lit),
            Pattern::Wildcard => self.output.push('_'),
            Pattern::Tuple(patterns, has_rest) => {
                self.output.push('[');
                self.comma_sep(patterns, Unparser::unparse_pattern);
                if *has_rest {
                    if !patterns.is_empty() {
                        self.output.push_str(", ");
                    }
                    self.output.push_str("..");
                }
                self.output.push(']');
            }
            Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                ..
            } => {
                if let Some(qualifier) = variant_qualifier {
                    self.unparse_type(qualifier);
                    self.output.push_str("::");
                }
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    self.delimited("(", ")", bindings, Unparser::unparse_pattern);
                }
            }
            Pattern::Struct {
                type_name,
                fields,
                has_rest,
                ..
            } => {
                if let Some(name) = type_name {
                    self.output.push_str(name);
                    self.output.push(' ');
                }
                self.output.push_str("{ ");
                self.comma_sep(fields, |s, field| {
                    let bare_name = is_bare_field_name(&field.field_name);
                    s.output.push_str(&format_field_name(&field.field_name));
                    let is_shorthand = bare_name
                        && matches!(&field.pattern, Pattern::Ident { name: n, .. } if n == &field.field_name);
                    if !is_shorthand {
                        s.output.push_str(": ");
                        s.unparse_pattern(&field.pattern);
                    }
                });
                if *has_rest {
                    if !fields.is_empty() {
                        self.output.push_str(", ");
                    }
                    self.output.push_str("..");
                }
                self.output.push_str(" }");
            }
            Pattern::Or(alternatives) => {
                self.comma_sep_with(" | ", alternatives, Unparser::unparse_pattern);
            }
            Pattern::Range {
                start, end, kind, ..
            } => {
                self.unparse_pattern(start);
                match kind {
                    crate::ast::RangeKind::Exclusive => self.output.push_str("..<"),
                    crate::ast::RangeKind::Inclusive => self.output.push_str("..="),
                }
                self.unparse_pattern(end);
            }
            // The formatter is fail-fast on syntax errors, so this placeholder
            // is never reached; emit nothing to keep the match total.
            Pattern::Error(_) => {}
        }
    }

    fn unparse_closure(&mut self, c: &ClosureExpr) {
        self.delimited("|", "| ", &c.params, |s, param| {
            s.emit_kw_if(param.is_mut, "mut ");
            s.output.push_str(&param.name);
            if let Some(ty) = &param.ty {
                s.output.push_str(": ");
                s.unparse_type(ty);
            }
        });
        self.unparse_expr(&c.body);
    }

    fn unparse_template_string(&mut self, t: &TemplateStringExpr) {
        use crate::ast::TemplatePart;

        self.output.push('`');
        for part in &t.parts {
            match part {
                TemplatePart::String(s) => {
                    escape_template_literal_into(s, &mut self.output);
                }
                TemplatePart::Interpolation { expr, format } => {
                    self.output.push('{');
                    self.unparse_expr(expr);
                    if let Some(fmt) = format {
                        self.output.push(':');
                        self.output.push_str(&fmt.spec);
                    }
                    self.output.push('}');
                }
            }
        }
        self.output.push('`');
    }

    fn unparse_cast(&mut self, c: &CastExpr) {
        let needs_parens = matches!(
            &c.expr,
            Expr::Binary(_) | Expr::Assign(_) | Expr::CompoundAssign(_)
        );
        self.with_parens_if(needs_parens, |s| s.unparse_expr(&c.expr));
        self.output.push_str(" as ");
        self.unparse_type(&c.target_type);
    }

    fn unparse_struct_literal(&mut self, s: &StructLiteralExpr) {
        if let Some(name) = &s.name {
            self.output.push_str(name);
            self.output.push(' ');
        }

        if s.fields.is_empty() {
            self.output.push_str("{}");
            return;
        }

        // A struct literal breaks one field per line — never fills — when the
        // source asked for it (trailing comma), when a field value is a nested
        // container (depth rule), or when more than one field bears a call. A
        // flat, single-call-at-most struct is inline-first and only wraps on
        // width.
        let has_container = s.fields.iter().any(|f| expr_is_container(&f.value));
        let call_fields = s.fields.iter().filter(|f| contains_call(&f.value)).count();
        if !s.has_trailing_comma && !has_container && call_fields <= 1 {
            let snap = self.snapshot();
            self.output.push_str("{ ");
            self.comma_sep(&s.fields, Unparser::emit_struct_literal_field);
            self.output.push_str(" }");

            if s.fields.len() <= 1 || !self.exceeds_width_since(snap) {
                return;
            }
            self.rollback(snap);
        }

        self.output.push_str("{\n");
        self.indent_level += 1;
        for field in &s.fields {
            self.write_indent();
            self.emit_struct_literal_field(field);
            self.output.push_str(",\n");
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    fn emit_struct_literal_field(&mut self, field: &crate::ast::StructLiteralField) {
        self.output.push_str(&format_field_name(&field.name));
        if !field.is_shorthand {
            self.output.push_str(": ");
            self.unparse_expr(&field.value);
        }
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("    ");
        }
    }

    /// Emit `f(self, item)` for each item separated by `", "`.
    fn comma_sep<I, F>(&mut self, items: I, f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        self.comma_sep_with(", ", items, f);
    }

    /// Like `comma_sep`, but with a custom `separator` between items.
    fn comma_sep_with<I, F>(&mut self, separator: &str, items: I, mut f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                self.output.push_str(separator);
            }
            f(self, item);
        }
    }

    /// Emit `open`, then `f(self, item)` for each item separated by `", "`, then `close`.
    fn delimited<I, F>(&mut self, open: &str, close: &str, items: I, f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        self.output.push_str(open);
        self.comma_sep(items, f);
        self.output.push_str(close);
    }

    /// Emit `kw` if `cond` is true. Used for modifiers like `pub `, `mut `, `async `.
    fn emit_kw_if(&mut self, cond: bool, kw: &str) {
        if cond {
            self.output.push_str(kw);
        }
    }

    /// Run `f`; wrap its output in `( ... )` when `cond` is true. Used to
    /// disambiguate operator precedence around recursively-emitted expressions.
    fn with_parens_if<F>(&mut self, cond: bool, f: F)
    where
        F: FnOnce(&mut Self),
    {
        if cond {
            self.output.push('(');
        }
        f(self);
        if cond {
            self.output.push(')');
        }
    }

    /// Emit each outer attribute on its own indented line, then write the indent
    /// for the line that follows. Replaces the `write_indent + for attr {...}` prologue
    /// shared by every item and member declaration.
    fn emit_outer_attrs(&mut self, attrs: &[Attribute]) {
        for attr in attrs {
            self.write_indent();
            self.unparse_attribute(attr);
            self.output.push('\n');
        }
        self.write_indent();
    }

    /// Emit ` {`, run `body`, then close with the matching `}` on its own indented
    /// line. The body callback runs at one extra indent level. This captures the
    /// shared shape of `struct`/`enum`/`variant`/`flags`/`interface`/`world` bodies.
    /// `outer_span` is the AST span of the enclosing declaration (used to track
    /// blank lines around members).
    fn with_braced_body<F>(&mut self, outer_span: Span, body: F)
    where
        F: FnOnce(&mut Self),
    {
        self.output.push_str(" {\n");
        self.indent_level += 1;
        let saved_line = self.last_source_line;
        self.last_source_line = outer_span.line;

        body(self);

        self.indent_level -= 1;
        self.last_source_line = saved_line.max(outer_span.end_line());
        self.write_indent();
        self.output.push_str("}\n");
    }

    /// Emit a member of a braced body that itself carries leading attributes and
    /// optional comments: leading comments → blank lines → caller body → inline
    /// trailing comments → newline.
    fn emit_member<F>(&mut self, id: crate::ast::AstId, span: Span, attrs: &[Attribute], body: F)
    where
        F: FnOnce(&mut Self),
    {
        let effective_line = effective_start_line(attrs, span.line);
        self.emit_leading_for(id);
        self.emit_blank_lines_to(effective_line);
        body(self);
        self.emit_trailing_for_inline(id);
        self.output.push('\n');
        self.last_source_line = span.end_line();
    }

    /// Save current output position for snapshot/rollback.
    fn snapshot(&self) -> usize {
        self.output.len()
    }

    /// Rollback output to a saved snapshot.
    fn rollback(&mut self, snapshot: usize) {
        self.output.truncate(snapshot);
    }

    /// Check if any line since snapshot exceeds `MAX_LINE_WIDTH`.
    fn exceeds_width_since(&self, snapshot: usize) -> bool {
        let added = &self.output[snapshot..];
        added.split('\n').any(|line| {
            if line.as_ptr() == added.as_ptr() {
                // First chunk: column = position before snapshot + this chunk
                self.output[..snapshot]
                    .rfind('\n')
                    .map_or(snapshot + line.len(), |nl| snapshot - nl - 1 + line.len())
                    > MAX_LINE_WIDTH
            } else {
                line.len() > MAX_LINE_WIDTH
            }
        })
    }

    /// Emit `trivia.leading_of(id)` on its own indented line with the
    /// usual blank-line padding before each entry.
    fn emit_leading_for(&mut self, id: crate::ast::AstId) {
        let comments: Vec<Comment> = self.leading_of(id).to_vec();
        for comment in &comments {
            if self.emitted_comments.insert(comment.span.start) {
                self.emit_blank_lines_to(comment.span.line);
                self.write_indent();
                self.emit_comment(comment);
                self.output.push('\n');
                self.last_source_line = comment.span.line;
            }
        }
    }

    /// Like [`Self::emit_leading_for`] but returns whether the last
    /// emitted comment was a `///` doc comment. Items use this to
    /// decide whether to insert a blank line between leading docs and
    /// the item itself.
    fn emit_leading_for_check_doc(&mut self, id: crate::ast::AstId) -> bool {
        let comments: Vec<Comment> = self.leading_of(id).to_vec();
        let mut last_was_doc = false;
        for comment in &comments {
            if self.emitted_comments.insert(comment.span.start) {
                self.emit_blank_lines_to(comment.span.line);
                self.write_indent();
                self.emit_comment(comment);
                self.output.push('\n');
                self.last_source_line = comment.span.line;
                last_was_doc = comment.kind == CommentKind::DocLine;
            }
        }
        last_was_doc
    }

    /// Emit `trivia.trailing_of(id)` after the node, inserting it
    /// *before* a trailing newline if the output already ended with one
    /// — keeping the comment glued to the node on the same line.
    fn emit_trailing_for(&mut self, id: crate::ast::AstId) {
        let comments: Vec<Comment> = self.trailing_of(id).to_vec();
        for comment in &comments {
            if self.emitted_comments.insert(comment.span.start) {
                if self.output.ends_with('\n') {
                    self.output.pop();
                    self.output.push_str("  ");
                    self.emit_comment(comment);
                    self.output.push('\n');
                } else {
                    self.output.push_str("  ");
                    self.emit_comment(comment);
                }
            }
        }
    }

    /// Inline variant of [`Self::emit_trailing_for`]: appends two
    /// spaces and the comment without touching surrounding newlines.
    /// Used in places where the caller manages line termination itself.
    fn emit_trailing_for_inline(&mut self, id: crate::ast::AstId) {
        let comments: Vec<Comment> = self.trailing_of(id).to_vec();
        for comment in &comments {
            if self.emitted_comments.insert(comment.span.start) {
                self.output.push_str("  ");
                self.emit_comment(comment);
            }
        }
    }

    /// Returns true iff any node nested inside the match expression
    /// `m` has any leading, trailing, or inner-tail trivia attached —
    /// i.e. any comment lives strictly inside `m`'s source range.
    /// Used by `try_inline_match` to bail out of single-line rendering
    /// when collapsing arms onto one line would silently drop comments.
    fn subtree_has_trivia(&self, m: &MatchExpr) -> bool {
        let trivia = match self.trivia {
            Some(t) => t,
            None => return false,
        };
        struct Probe<'t> {
            trivia: &'t crate::comment::TriviaMap,
            found: bool,
        }
        impl crate::ast::AstVisitor for Probe<'_> {
            fn visit_id(&mut self, id: crate::ast::AstId, _span: Span) {
                if self.found {
                    return;
                }
                if !self.trivia.leading_of(id).is_empty()
                    || !self.trivia.trailing_of(id).is_empty()
                    || !self.trivia.inner_tail_of(id).is_empty()
                {
                    self.found = true;
                }
            }
        }
        let mut probe = Probe {
            trivia,
            found: false,
        };
        // `walk_match_expr` deliberately omits `m.id` itself (its caller
        // emits it). For this probe we only care about *interior*
        // trivia, so leaving `m.id` out is exactly the behaviour we
        // want — leading/trailing of `m` itself live outside `m.span`.
        crate::ast::AstVisitor::visit_match_expr(&mut probe, m);
        probe.found
    }

    /// Flush comments that fall inside a block but after its last
    /// statement (its `inner_tail`), each on its own indented line.
    fn emit_inner_tail_for(&mut self, id: crate::ast::AstId) {
        let comments: Vec<Comment> = self.inner_tail_of(id).to_vec();
        for comment in &comments {
            if self.emitted_comments.insert(comment.span.start) {
                self.emit_blank_lines_to(comment.span.line);
                self.write_indent();
                self.emit_comment(comment);
                self.output.push('\n');
                self.last_source_line = comment.span.line;
            }
        }
    }

    fn emit_comment(&mut self, comment: &Comment) {
        match comment.kind {
            CommentKind::Line => {
                self.output.push_str("//");
                self.output.push_str(&comment.text);
            }
            CommentKind::Block => {
                self.output.push_str("/*");
                self.output.push_str(&comment.text);
                self.output.push_str("*/");
            }
            CommentKind::DocLine => {
                self.output.push_str("///");
                self.output.push_str(&comment.text);
            }
            CommentKind::ModuleDoc => {
                self.output.push_str("//!");
                self.output.push_str(&comment.text);
            }
        }
    }
}

pub fn get_item_span(item: &Item) -> Span {
    match item {
        Item::Use(u) => u.span,
        Item::Function(f) => f.span,
        Item::Struct(s) => s.span,
        Item::Enum(e) => e.span,
        Item::Variant(v) => v.span,
        Item::Flags(f) => f.span,
        Item::Newtype(t) => t.span,
        Item::Impl(i) => i.span,
        Item::Trait(t) => t.span,
        Item::Interface(e) => e.span,
        Item::Resource(r) => r.span,
        Item::World(w) => w.span,
        Item::Test(t) => t.span,
        Item::Global(g) => g.span,
        Item::TupleTypeDecl(d) => d.span,
        Item::BuiltinTypeDecl(d) => d.span,
        Item::Error(e) => e.span,
    }
}

pub fn get_item_id(item: &Item) -> crate::ast::AstId {
    match item {
        Item::Use(u) => u.id,
        Item::Function(f) => f.id,
        Item::Struct(s) => s.id,
        Item::Enum(e) => e.id,
        Item::Variant(v) => v.id,
        Item::Flags(f) => f.id,
        Item::Newtype(t) => t.id,
        Item::Impl(i) => i.id,
        Item::Trait(t) => t.id,
        Item::Interface(e) => e.id,
        Item::Resource(r) => r.id,
        Item::World(w) => w.id,
        Item::Test(t) => t.id,
        Item::Global(g) => g.id,
        Item::TupleTypeDecl(d) => d.id,
        Item::BuiltinTypeDecl(d) => d.id,
        Item::Error(e) => e.id,
    }
}

/// Returns the first source line of an item, including any preceding attributes.
/// This is used to compute blank lines correctly when items have both doc comments
/// and attributes, avoiding blank-line growth on repeated formatting passes.
fn get_item_first_line(item: &Item) -> usize {
    let first_attr_line = |attrs: &[crate::ast::Attribute]| attrs.first().map(|a| a.span.line);
    let item_line = get_item_span(item).line;
    let attr_line = match item {
        Item::Struct(s) => first_attr_line(&s.attrs),
        Item::Enum(e) => first_attr_line(&e.attrs),
        Item::Variant(v) => first_attr_line(&v.attrs),
        Item::Interface(e) => first_attr_line(&e.attrs),
        Item::Resource(r) => first_attr_line(&r.attrs),
        Item::Function(f) => first_attr_line(&f.attrs),
        Item::Newtype(t) => first_attr_line(&t.attrs),
        Item::World(w) => first_attr_line(&w.attrs),
        Item::Global(g) => first_attr_line(&g.attributes),
        Item::Flags(f) => f
            .attributes
            .as_deref()
            .and_then(|a| a.first().map(|a| a.span.line)),
        Item::Trait(t) => first_attr_line(&t.attrs),
        Item::TupleTypeDecl(d) => first_attr_line(&d.attrs),
        Item::BuiltinTypeDecl(d) => first_attr_line(&d.attrs),
        Item::Test(t) => first_attr_line(&t.attributes),
        _ => None,
    };
    attr_line.unwrap_or(item_line).min(item_line)
}

fn get_stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::Let(l) => l.span,
        Stmt::Expr(e) => e.span,
        Stmt::Return(r) => r.span,
        Stmt::TaskReturn(tr) => tr.span,
        Stmt::If(i) => i.span,
        Stmt::While(w) => w.span,
        Stmt::For(f) => f.span,
        Stmt::ForOf(f) => f.span,
        Stmt::Loop(l) => l.span,
        Stmt::Match(m) => m.span,
        Stmt::Break(b) => b.span,
        Stmt::Continue(c) => c.span,
        Stmt::Assert(a) => a.span,
        Stmt::LabeledBlock(lb) => lb.span,
        Stmt::Error(s) => s.span,
    }
}

fn block_single_expr(block: &Block) -> Option<&Expr> {
    if block.stmts.len() == 1
        && let Stmt::Expr(e) = &block.stmts[0]
    {
        return Some(&e.expr);
    }
    None
}

fn is_inline_safe_expr(expr: &Expr) -> bool {
    !matches!(
        expr,
        Expr::Block(_) | Expr::If(_) | Expr::Match(_) | Expr::Closure(_) | Expr::LabeledBlock(_)
    )
}

fn binary_op_str(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Eq => "==",
        BinaryOp::NotEq => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "^",
        BinaryOp::Shl => "<<",
        BinaryOp::Shr => ">>",
    }
}

fn compound_op_str(op: CompoundAssignOp) -> &'static str {
    match op {
        CompoundAssignOp::Add => "+=",
        CompoundAssignOp::Sub => "-=",
        CompoundAssignOp::Mul => "*=",
        CompoundAssignOp::Div => "/=",
        CompoundAssignOp::Mod => "%=",
        CompoundAssignOp::BitAnd => "&=",
        CompoundAssignOp::BitOr => "|=",
        CompoundAssignOp::BitXor => "^=",
        CompoundAssignOp::Shl => "<<=",
        CompoundAssignOp::Shr => ">>=",
    }
}

fn unary_op_str(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "!",
        UnaryOp::BitNot => "~",
        UnaryOp::Ref => "&",
        UnaryOp::MutRef => "&mut ",
        UnaryOp::Deref => "*",
    }
}

fn binary_op_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 2,
        BinaryOp::And => 3,
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => 4,
        BinaryOp::BitOr => 5,
        BinaryOp::BitXor => 6,
        BinaryOp::BitAnd => 7,
        BinaryOp::Shl | BinaryOp::Shr => 8,
        BinaryOp::Add | BinaryOp::Sub => 9,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 10,
    }
}

/// Collect all operands of a same-op logical chain (flattens left- and right-associative trees).
fn collect_logical_chain_binary(b: &BinaryExpr) -> Vec<&Expr> {
    let mut parts = Vec::new();
    collect_logical_chain_expr(&b.left, b.op, &mut parts);
    collect_logical_chain_expr(&b.right, b.op, &mut parts);
    parts
}

fn collect_logical_chain_expr<'a>(expr: &'a Expr, op: BinaryOp, parts: &mut Vec<&'a Expr>) {
    match expr {
        Expr::Binary(inner) if inner.op == op => {
            collect_logical_chain_expr(&inner.left, op, parts);
            collect_logical_chain_expr(&inner.right, op, parts);
        }
        _ => parts.push(expr),
    }
}

fn needs_parens(expr: &Expr, parent_op: BinaryOp, is_left: bool) -> bool {
    match expr {
        Expr::Binary(inner) => {
            let inner_prec = binary_op_precedence(inner.op);
            let parent_prec = binary_op_precedence(parent_op);

            if inner_prec < parent_prec {
                return true;
            }
            // Comparison operators chain instead of associating: `a == b == c`
            // parses as the 3-way chain `a == b && b == c`, and mixing groups
            // (`a < b < c == d`) is a parse error. A comparison nested as
            // either operand of another comparison must keep its parens, or the
            // round-trip silently rewrites the meaning. The same-precedence arm
            // below would otherwise drop them for the left operand.
            if is_comparison_op(inner.op) && is_comparison_op(parent_op) {
                return true;
            }
            if inner_prec == parent_prec && !is_left {
                // Right-associative check for same precedence
                return true;
            }
            false
        }
        // A comparison chain (`a < b < c`) behaves like a comparison operand:
        // it must stay parenthesized inside any operator binding at least as
        // tightly as comparison, and inside another comparison (which would
        // extend or invalidate the chain). Only the looser-binding logical
        // `&&` / `||` can hold a bare chain.
        Expr::ComparisonChain(_) => !matches!(parent_op, BinaryOp::And | BinaryOp::Or),
        // Range expressions have lower precedence than all binary operators,
        // so they always need parentheses when nested inside a binary expression.
        Expr::Range(_) => true,
        // `x as T < y` is re-parsed as `x as T<y>` (generic type), so the cast
        // must be parenthesized when it appears as the left operand of `<`.
        Expr::Cast(_) if is_left && parent_op == BinaryOp::Lt => true,
        _ => false,
    }
}

/// Comparison operators chain (`a < b < c`) rather than associate, so they
/// need parenthesization rules distinct from ordinary same-precedence binaries.
fn is_comparison_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
    )
}

/// Returns true if `name` can be emitted as a bare identifier or keyword in a
/// struct-literal / struct-pattern field position. Otherwise the field name
/// must be rendered as a quoted string literal (for JSON compatibility).
fn is_bare_field_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Emit a struct-literal / struct-pattern field name, wrapping it in a string
/// literal if it is not a valid bare identifier.
fn format_field_name(name: &str) -> String {
    if is_bare_field_name(name) {
        name.to_string()
    } else {
        format!("\"{}\"", escape_string(name))
    }
}

fn escape_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{{{:04X}}}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

fn escape_char(c: char) -> String {
    match c {
        '\'' => "\\'".to_string(),
        '\\' => "\\\\".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        '\0' => "\\0".to_string(),
        c if c.is_control() => format!("\\u{{{:04X}}}", c as u32),
        c => c.to_string(),
    }
}

fn format_spec_to_string(spec: &crate::tir::TemplateFormatSpec) -> String {
    let mut s = String::new();
    if let Some(fill) = spec.fill {
        s.push(fill);
    }
    if let Some(align) = spec.align {
        s.push(align);
    }
    if spec.sign_plus {
        s.push('+');
    }
    if spec.alternate {
        s.push('#');
    }
    if spec.zero_pad {
        s.push('0');
    }
    if let Some(w) = spec.width {
        s.push_str(&w.to_string());
    }
    if let Some(p) = spec.precision {
        s.push('.');
        s.push_str(&p.to_string());
    }
    if let Some(t) = spec.type_char {
        s.push(t);
    }
    s
}

/// Unparse an AST expression to a string without comments.
/// Used by the desugar phase for generating error messages.
pub fn unparse_expr_simple(expr: &Expr) -> String {
    let mut output = String::new();
    unparse_expr_into(expr, &mut output);
    output
}

/// Unparse an expression to a string without preserving comments. The output
/// drops disambiguating parentheses around nested binary expressions: callers
/// (error messages, simple symbol previews) prioritise readability over
/// round-trip fidelity.
fn unparse_expr_into(expr: &Expr, output: &mut String) {
    match expr {
        Expr::Ident(i) => {
            output.push_str(&i.name);
            if !i.type_args.is_empty() {
                output.push_str("::<");
                for (idx, ty) in i.type_args.iter().enumerate() {
                    if idx > 0 {
                        output.push_str(", ");
                    }
                    unparse_type_into(ty, output);
                }
                output.push('>');
            }
        }
        Expr::Literal(l) => unparse_literal_into(&l.value, output),
        Expr::Binary(b) => {
            unparse_expr_into(&b.left, output);
            output.push(' ');
            output.push_str(binary_op_str(b.op));
            output.push(' ');
            unparse_expr_into(&b.right, output);
        }
        Expr::Unary(u) => {
            output.push_str(unary_op_str(u.op));
            unparse_expr_into(&u.expr, output);
        }
        Expr::Call(c) => {
            unparse_expr_into(&c.callee, output);
            delimited_into("(", ")", &c.args, output, unparse_expr_into);
        }
        Expr::MethodCall(m) => {
            unparse_expr_into(&m.receiver, output);
            output.push('.');
            output.push_str(&m.method);
            delimited_into("(", ")", &m.args, output, unparse_expr_into);
        }
        Expr::FieldAccess(f) => {
            unparse_expr_into(&f.expr, output);
            output.push('.');
            output.push_str(&f.field);
        }
        Expr::Index(i) => {
            unparse_expr_into(&i.expr, output);
            output.push('[');
            unparse_expr_into(&i.index, output);
            output.push(']');
        }
        Expr::Cast(c) => {
            unparse_expr_into(&c.expr, output);
            output.push_str(" as ");
            unparse_type_into(&c.target_type, output);
        }
        Expr::StaticMethodCall(s) => {
            unparse_type_into(&s.target_type, output);
            output.push_str("::");
            output.push_str(&s.method);
            if !s.type_args.is_empty() {
                delimited_into("::<", ">", &s.type_args, output, unparse_type_into);
            }
            delimited_into("(", ")", &s.args, output, unparse_expr_into);
        }
        Expr::Closure(c) => unparse_closure_into(c, output),
        Expr::TemplateString(t) => unparse_template_string_into(t, output),
        Expr::Block(b) => unparse_block_expr_into(b, output),
        Expr::If(i) => unparse_if_expr_into(i, output),
        Expr::Match(m) => unparse_match_into(m, output),
        Expr::Matches(m) => {
            unparse_expr_into(&m.expr, output);
            output.push_str(" matches { ");
            unparse_pattern_into(&m.pattern, output);
            if let Some(guard) = &m.guard {
                output.push_str(" && ");
                unparse_expr_into(guard, output);
            }
            output.push_str(" }");
        }
        Expr::Assign(a) => {
            unparse_expr_into(&a.target, output);
            output.push_str(" = ");
            unparse_expr_into(&a.value, output);
        }
        Expr::CompoundAssign(ca) => {
            unparse_expr_into(&ca.target, output);
            output.push_str(match ca.op {
                CompoundAssignOp::Add => " += ",
                CompoundAssignOp::Sub => " -= ",
                CompoundAssignOp::Mul => " *= ",
                CompoundAssignOp::Div => " /= ",
                CompoundAssignOp::Mod => " %= ",
                CompoundAssignOp::BitAnd => " &= ",
                CompoundAssignOp::BitOr => " |= ",
                CompoundAssignOp::BitXor => " ^= ",
                CompoundAssignOp::Shl => " <<= ",
                CompoundAssignOp::Shr => " >>= ",
            });
            unparse_expr_into(&ca.value, output);
        }
        Expr::ComparisonChain(chain) => {
            unparse_expr_into(&chain.first, output);
            for cmp in &chain.comparisons {
                output.push(' ');
                output.push_str(binary_op_str(cmp.op));
                output.push(' ');
                unparse_expr_into(&cmp.right, output);
            }
        }
        Expr::StructLiteral(s) => {
            if let Some(name) = &s.name {
                output.push_str(name);
                output.push(' ');
            }
            if s.fields.is_empty() {
                output.push_str("{}");
            } else {
                output.push_str("{ ");
                comma_sep_into(&s.fields, output, |f, o| {
                    o.push_str(&f.name);
                    if !f.is_shorthand {
                        o.push_str(": ");
                        unparse_expr_into(&f.value, o);
                    }
                });
                output.push_str(" }");
            }
        }
        Expr::TupleLiteral(t) => {
            delimited_into("[", "]", &t.elements, output, unparse_expr_into);
        }
        Expr::LabeledBlock(_) => output.push_str("<labeled-block>"),
        Expr::TryOp(qm) => {
            unparse_expr_into(&qm.expr, output);
            output.push('?');
        }
        Expr::Spread(inner, _) => {
            output.push_str("..");
            unparse_expr_into(inner, output);
        }
        Expr::Range(range) => {
            unparse_expr_into(&range.start, output);
            match range.kind {
                crate::ast::RangeKind::Exclusive => output.push_str("..<"),
                crate::ast::RangeKind::Inclusive => output.push_str("..="),
            }
            unparse_expr_into(&range.end, output);
        }
        Expr::WithHandler(w) => {
            output.push_str("with ");
            comma_sep_into(&w.handlers, output, |binding, o| {
                if let Some(effect) = &binding.effect {
                    unparse_type_into(effect, o);
                    o.push_str(" => ");
                }
                unparse_expr_into(&binding.handler, o);
            });
            output.push_str(" do ");
            unparse_block_expr_into(&w.body, output);
        }
        Expr::Resume(r) => {
            output.push_str("resume ");
            unparse_expr_into(&r.value, output);
        }
        // Parser error-recovery placeholder; rendered as an empty marker for
        // the readability-first preview paths that use this helper.
        Expr::Error(_) => output.push_str("<error>"),
    }
}

fn unparse_closure_into(c: &ClosureExpr, output: &mut String) {
    delimited_into("|", "| ", &c.params, output, |param, o| {
        if param.is_mut {
            o.push_str("mut ");
        }
        o.push_str(&param.name);
        if let Some(ty) = &param.ty {
            o.push_str(": ");
            unparse_type_into(ty, o);
        }
    });
    unparse_expr_into(&c.body, output);
}

fn escape_template_literal_into(s: &str, output: &mut String) {
    // Template literal parts are stored as raw text (escape sequences preserved).
    // Just output as-is — escapes like \n, \{, \} are already in raw form.
    output.push_str(s);
}

fn unparse_template_string_into(t: &TemplateStringExpr, output: &mut String) {
    use crate::ast::TemplatePart;
    output.push('`');
    for part in &t.parts {
        match part {
            TemplatePart::String(s) => {
                escape_template_literal_into(s, output);
            }
            TemplatePart::Interpolation { expr, format } => {
                output.push('{');
                unparse_expr_into(expr, output);
                if let Some(fmt) = format {
                    output.push(':');
                    output.push_str(&fmt.spec);
                }
                output.push('}');
            }
        }
    }
    output.push('`');
}

fn unparse_block_expr_into(b: &Block, output: &mut String) {
    if b.stmts.is_empty() {
        output.push_str("{}");
        return;
    }
    output.push_str("{ ");
    for (i, stmt) in b.stmts.iter().enumerate() {
        if i > 0 {
            // Stmt::Let etc. terminate with `;`, so this single space keeps the
            // single-line debug form readable: `{ let x = 1; foo(x) }`.
            output.push(' ');
        }
        unparse_stmt_into(stmt, output);
    }
    output.push_str(" }");
}

fn unparse_stmt_into(stmt: &Stmt, output: &mut String) {
    match stmt {
        Stmt::Let(l) => {
            output.push_str("let ");
            if l.is_mut {
                output.push_str("mut ");
            }
            unparse_pattern_into(&l.pattern, output);
            if let Some(ty) = &l.ty {
                output.push_str(": ");
                unparse_type_into(ty, output);
            }
            if let Some(ref v) = l.value {
                output.push_str(" = ");
                unparse_expr_into(v, output);
            }
            output.push(';');
        }
        Stmt::Expr(e) => {
            unparse_expr_into(&e.expr, output);
            output.push(';');
        }
        Stmt::Return(r) => {
            output.push_str("return");
            if let Some(v) = &r.value {
                output.push(' ');
                unparse_expr_into(v, output);
            }
            output.push(';');
        }
        Stmt::If(i) => {
            output.push_str("if ");
            unparse_condition_into(&i.condition, output);
            output.push(' ');
            unparse_block_expr_into(&i.then_block, output);
            if let Some(else_block) = &i.else_block {
                output.push_str(" else ");
                unparse_block_expr_into(else_block, output);
            }
        }
        Stmt::While(w) => {
            output.push_str("while ");
            unparse_condition_into(&w.condition, output);
            output.push(' ');
            unparse_block_expr_into(&w.body, output);
        }
        Stmt::For(f) => {
            output.push_str("for ");
            if let Some(init) = &f.init {
                unparse_stmt_into(init, output);
            }
            output.push(' ');
            if let Some(cond) = &f.condition {
                unparse_condition_into(cond, output);
            }
            output.push_str("; ");
            if let Some(update) = &f.update {
                unparse_expr_into(update, output);
            }
            output.push(' ');
            unparse_block_expr_into(&f.body, output);
        }
        Stmt::ForOf(f) => {
            output.push_str("for let ");
            if f.is_mut {
                output.push_str("mut ");
            }
            unparse_pattern_into(&f.binding, output);
            output.push_str(" of ");
            unparse_expr_into(&f.iterable, output);
            output.push(' ');
            unparse_block_expr_into(&f.body, output);
        }
        Stmt::Loop(l) => {
            output.push_str("loop ");
            unparse_block_expr_into(&l.body, output);
        }
        Stmt::Match(m) => {
            unparse_match_into(m, output);
        }
        Stmt::Break(_) => output.push_str("break;"),
        Stmt::Continue(_) => output.push_str("continue;"),
        Stmt::Assert(a) => {
            output.push_str("assert ");
            unparse_expr_into(&a.condition, output);
            if let Some(msg) = &a.message {
                output.push_str(", ");
                unparse_expr_into(msg, output);
            }
            output.push(';');
        }
        Stmt::TaskReturn(tr) => {
            output.push_str("task return ");
            unparse_expr_into(&tr.value, output);
            output.push(';');
        }
        Stmt::LabeledBlock(lb) => {
            output.push_str(&lb.label);
            output.push_str(": ");
            unparse_block_expr_into(&lb.block, output);
        }
        // Parser error-recovery placeholder; rendered as an empty marker for the
        // readability-first preview paths that use this helper.
        Stmt::Error(_) => output.push_str("<error>;"),
    }
}

fn unparse_condition_into(cond: &Condition, output: &mut String) {
    match cond {
        Condition::Expr(e) => unparse_expr_into(e, output),
        Condition::LetChain { elements, .. } => {
            comma_sep_with_into(" && ", elements, output, |elem, output| match elem {
                ConditionElement::Let { pattern, expr, .. } => {
                    output.push_str("let ");
                    unparse_pattern_into(pattern, output);
                    output.push_str(" = ");
                    unparse_expr_into(expr, output);
                }
                ConditionElement::Expr(expr) => {
                    let needs_parens = matches!(
                        expr,
                        Expr::Binary(b) if matches!(b.op, BinaryOp::And | BinaryOp::Or)
                    );
                    if needs_parens {
                        output.push('(');
                    }
                    unparse_expr_into(expr, output);
                    if needs_parens {
                        output.push(')');
                    }
                }
            });
        }
    }
}

fn unparse_pattern_into(pattern: &Pattern, output: &mut String) {
    match pattern {
        Pattern::Ident { name, .. } => output.push_str(name),
        Pattern::MutIdent { name, .. } => {
            output.push_str("mut ");
            output.push_str(name);
        }
        Pattern::Literal(lit) => unparse_literal_into(lit, output),
        Pattern::Wildcard => output.push('_'),
        Pattern::Tuple(pats, has_rest) => {
            output.push('[');
            comma_sep_into(pats, output, unparse_pattern_into);
            if *has_rest {
                if !pats.is_empty() {
                    output.push_str(", ");
                }
                output.push_str("..");
            }
            output.push(']');
        }
        Pattern::Variant {
            variant_name,
            variant_qualifier,
            bindings,
            ..
        } => {
            if let Some(qualifier) = variant_qualifier {
                unparse_type_into(qualifier, output);
                output.push_str("::");
            }
            output.push_str(variant_name);
            if !bindings.is_empty() {
                delimited_into("(", ")", bindings, output, unparse_pattern_into);
            }
        }
        Pattern::Struct {
            type_name,
            fields,
            has_rest,
            ..
        } => {
            if let Some(name) = type_name {
                output.push_str(name);
                output.push(' ');
            }
            output.push_str("{ ");
            comma_sep_into(fields, output, |field, o| {
                let bare_name = is_bare_field_name(&field.field_name);
                o.push_str(&format_field_name(&field.field_name));
                let is_shorthand = bare_name
                    && matches!(&field.pattern, Pattern::Ident { name: n, .. } if n == &field.field_name);
                if !is_shorthand {
                    o.push_str(": ");
                    unparse_pattern_into(&field.pattern, o);
                }
            });
            if *has_rest {
                if !fields.is_empty() {
                    output.push_str(", ");
                }
                output.push_str("..");
            }
            output.push_str(" }");
        }
        Pattern::Or(alternatives) => {
            comma_sep_with_into(" | ", alternatives, output, unparse_pattern_into);
        }
        Pattern::Range {
            start, end, kind, ..
        } => {
            unparse_pattern_into(start, output);
            match kind {
                crate::ast::RangeKind::Exclusive => output.push_str("..<"),
                crate::ast::RangeKind::Inclusive => output.push_str("..="),
            }
            unparse_pattern_into(end, output);
        }
        Pattern::Error(_) => output.push_str("<error>"),
    }
}

fn unparse_if_expr_into(i: &IfExpr, output: &mut String) {
    output.push_str("if ");
    unparse_condition_into(&i.condition, output);
    output.push(' ');
    unparse_block_expr_into(&i.then_block, output);
    if let Some(else_block) = &i.else_block {
        output.push_str(" else ");
        unparse_block_expr_into(else_block, output);
    }
}

fn unparse_match_into(m: &MatchExpr, output: &mut String) {
    output.push_str("match ");
    unparse_expr_into(&m.expr, output);
    output.push_str(" { ");
    comma_sep_into(&m.arms, output, |arm, o| {
        unparse_pattern_into(&arm.pattern, o);
        if let Some(guard) = &arm.guard {
            o.push_str(" && ");
            unparse_expr_into(guard, o);
        }
        o.push_str(" => ");
        unparse_expr_into(&arm.body, o);
    });
    output.push_str(" }");
}

pub fn unparse_type_into(ty: &Type, output: &mut String) {
    match ty {
        Type::Named(n) => output.push_str(&n.name),
        Type::Generic(g) => {
            output.push_str(&g.name);
            delimited_into("<", ">", &g.args, output, unparse_type_into);
        }
        Type::Function(f) => {
            output.push_str(if f.is_mut { "fn mut" } else { "fn" });
            delimited_into("(", ")", &f.params, output, unparse_type_into);
            unparse_fn_return_into(&f.return_type, output);
            unparse_fn_type_with_clause_into(&f.effects, &f.stores, output);
        }
        Type::Tuple(types) => {
            delimited_into("[", "]", types, output, unparse_type_into);
        }
        Type::Reference(inner) => {
            output.push('&');
            unparse_type_into(inner, output);
        }
        Type::MutReference(inner) => {
            output.push_str("&mut ");
            unparse_type_into(inner, output);
        }
        Type::TypePackSpread(name, _) => {
            output.push_str("..");
            output.push_str(name);
        }
        Type::NamespacedGeneric(ng) => {
            output.push_str(&ng.namespace);
            output.push_str("::");
            output.push_str(&ng.name);
            if !ng.args.is_empty() {
                delimited_into("<", ">", &ng.args, output, unparse_type_into);
            }
        }
        Type::Error(_) => output.push_str("<error>"),
    }
}

fn unparse_literal_into(lit: &Literal, output: &mut String) {
    match lit {
        Literal::Number(repr) => output.push_str(repr),
        Literal::String(raw) => {
            output.push('"');
            output.push_str(raw);
            output.push('"');
        }
        Literal::Char(raw) => {
            output.push('\'');
            output.push_str(raw);
            output.push('\'');
        }
        Literal::Bool(b) => output.push_str(if *b { "true" } else { "false" }),
        Literal::Null => output.push_str("null"),
        Literal::Unit => output.push_str("()"),
        Literal::LocationFile => output.push_str("#file"),
        Literal::LocationLine => output.push_str("#line"),
        Literal::LocationFunction => output.push_str("#function"),
        Literal::DataSection => output.push_str("#data"),
        Literal::IncludeStr(path) => {
            output.push_str("#include_str(\"");
            output.push_str(path);
            output.push_str("\")");
        }
        Literal::IncludeBytes(path) => {
            output.push_str("#include_bytes(\"");
            output.push_str(path);
            output.push_str("\")");
        }
    }
}

/// Signature-only unparsers for AST declarations.
///
/// These produce a single-line textual signature suitable for hover, completion
/// detail, document symbols, etc. — without emitting the body, attributes, or
/// surrounding indentation. They are stateless (no trivia attachment
/// needed) and match the canonical source syntax of the language.
pub fn unparse_function_signature(f: &Function) -> String {
    let mut out = String::new();
    unparse_function_signature_into(f, &mut out);
    out
}

pub fn unparse_function_signature_into(f: &Function, output: &mut String) {
    emit_kw_if_into(f.is_pub, "pub ", output);
    emit_kw_if_into(f.is_export, "export ", output);
    emit_kw_if_into(f.is_async, "async ", output);
    output.push_str("fn ");
    output.push_str(&f.name);
    unparse_generic_params_into(&f.type_params, output);
    delimited_into("(", ")", &f.params, output, unparse_param_into);
    if let Some(ret) = &f.return_type
        && !is_unit_type(ret)
    {
        output.push_str(" -> ");
        unparse_type_into(ret, output);
    }
    unparse_with_clause_into(&f.effects, &f.stores, output);
}

/// Emit `[pub ]<keyword> <name>[<generics>]` into `out`.
fn emit_decl_header(
    is_pub: bool,
    keyword: &str,
    name: &str,
    type_params: &[GenericParam],
    out: &mut String,
) {
    emit_kw_if_into(is_pub, "pub ", out);
    out.push_str(keyword);
    out.push_str(name);
    unparse_generic_params_into(type_params, out);
}

pub fn unparse_struct_header(s: &StructDecl) -> String {
    let mut out = String::new();
    emit_decl_header(s.is_pub, "struct ", &s.name, &s.type_params, &mut out);
    out
}

pub fn unparse_enum_header(e: &EnumDecl) -> String {
    let mut out = String::new();
    emit_decl_header(e.is_pub, "enum ", &e.name, &e.type_params, &mut out);
    out
}

pub fn unparse_variant_header(v: &VariantDecl) -> String {
    let mut out = String::new();
    emit_decl_header(v.is_pub, "variant ", &v.name, &v.type_params, &mut out);
    out
}

pub fn unparse_flags_header(fl: &FlagsDecl) -> String {
    let mut out = String::new();
    emit_decl_header(fl.is_pub, "flags ", &fl.name, &[], &mut out);
    out
}

pub fn unparse_trait_header(t: &TraitDecl) -> String {
    let mut out = String::new();
    emit_decl_header(t.is_pub, "trait ", &t.name, &t.type_params, &mut out);
    out
}

pub fn unparse_newtype_signature(n: &Newtype) -> String {
    let mut out = String::new();
    emit_decl_header(n.is_pub, "type ", &n.name, &n.type_params, &mut out);
    out.push_str(" = ");
    unparse_type_into(&n.ty, &mut out);
    out
}

pub fn unparse_builtin_type_decl_signature(d: &BuiltinTypeDecl) -> String {
    let mut out = String::new();
    emit_decl_header(d.is_pub, "type ", &d.name, &d.type_params, &mut out);
    out
}

pub fn unparse_global_signature(g: &GlobalDecl) -> String {
    let mut out = String::new();
    emit_kw_if_into(g.is_pub, "pub ", &mut out);
    out.push_str("global ");
    emit_kw_if_into(g.mutable, "mut ", &mut out);
    out.push_str(&g.name);
    out.push_str(": ");
    unparse_type_into(&g.ty, &mut out);
    out
}

pub fn unparse_generic_params_into(params: &[GenericParam], output: &mut String) {
    if params.is_empty() {
        return;
    }
    delimited_into("<", ">", params, output, |param, o| {
        emit_kw_if_into(param.is_effect, "effect ", o);
        emit_kw_if_into(param.is_pack, "..", o);
        o.push_str(&param.name);
        if !param.bounds.is_empty() {
            o.push_str(": ");
            for (j, bound) in param.bounds.iter().enumerate() {
                if j > 0 {
                    o.push_str(" + ");
                }
                if let Some(sig) = &bound.fn_signature {
                    unparse_fn_signature_in_bound_into(sig, o);
                } else {
                    o.push_str(&bound.name);
                    if !bound.assoc_types.is_empty() {
                        delimited_into("<", ">", &bound.assoc_types, o, |assoc, o| {
                            o.push_str(&assoc.name);
                            o.push_str(" = ");
                            unparse_type_into(&assoc.ty, o);
                        });
                    }
                }
            }
        }
        if let Some(default_type) = &param.default {
            o.push_str(" = ");
            unparse_type_into(default_type, o);
        }
    });
}

pub fn unparse_param_into(param: &Param, output: &mut String) {
    if let Some(self_form) = self_param_shorthand(param) {
        output.push_str(self_form);
        return;
    }
    emit_kw_if_into(param.is_mut, "mut ", output);
    output.push_str(&param.name);
    output.push_str(": ");
    unparse_type_into(&param.ty, output);
    if let Some(default) = &param.default {
        output.push_str(" = ");
        unparse_expr_into(default, output);
    }
}

/// Returns the shorthand rendering (`&self` / `&mut self`) for a parameter that
/// represents the receiver, or `None` if it should be rendered as a normal param.
/// Normalizes both the explicit `SelfKind` and the redundant `self: &Self` form.
fn self_param_shorthand(param: &Param) -> Option<&'static str> {
    match param.self_kind {
        SelfKind::Ref => return Some("&self"),
        SelfKind::MutRef => return Some("&mut self"),
        SelfKind::None => {}
    }
    if param.name != "self" {
        return None;
    }
    match &param.ty {
        Type::Reference(inner) if matches!(inner.as_ref(), Type::Named(n) if n.name == "Self") => {
            Some("&self")
        }
        Type::MutReference(inner) if matches!(inner.as_ref(), Type::Named(n) if n.name == "Self") => {
            Some("&mut self")
        }
        _ => None,
    }
}

pub fn unparse_with_clause_into(effects: &[String], stores: &[String], output: &mut String) {
    if effects.is_empty() && stores.is_empty() {
        return;
    }
    output.push_str(" with ");
    if !effects.is_empty() {
        output.push_str(&effects.join(", "));
        if !stores.is_empty() {
            output.push_str(", ");
        }
    }
    if !stores.is_empty() {
        output.push_str("stores[");
        output.push_str(&stores.join(", "));
        output.push(']');
    }
}

/// Emit ` -> <ret>` for a function type, omitting it entirely when the return
/// is the unit type — same rule as function declarations, so `fn mut(T)` and
/// `fn mut(T) -> ()` round-trip to the canonical arrowless form.
fn unparse_fn_return_into(return_type: &Type, output: &mut String) {
    if !is_unit_type(return_type) {
        output.push_str(" -> ");
        unparse_type_into(return_type, output);
    }
}

/// with-clause for function-type position (`stores[0, 1]` with positional indices).
/// Bound-context variant of `fn(...)` printing. Multi-effect `with` clauses
/// are parens-grouped because comma at this level separates trait bounds
/// (and `stores[...]` never appears in bound position).
fn unparse_fn_signature_in_bound_into(sig: &FunctionType, output: &mut String) {
    output.push_str(if sig.is_mut { "fn mut" } else { "fn" });
    delimited_into("(", ")", &sig.params, output, unparse_type_into);
    unparse_fn_return_into(&sig.return_type, output);
    match sig.effects.len() {
        0 => {}
        1 => {
            output.push_str(" with ");
            output.push_str(&sig.effects[0]);
        }
        _ => {
            output.push_str(" with (");
            output.push_str(&sig.effects.join(", "));
            output.push(')');
        }
    }
}

fn unparse_fn_type_with_clause_into(
    effects: &[String],
    stores: &[StoresEntry],
    output: &mut String,
) {
    if effects.is_empty() && stores.is_empty() {
        return;
    }
    output.push_str(" with ");
    if !effects.is_empty() {
        output.push_str(&effects.join(", "));
        if !stores.is_empty() {
            output.push_str(", ");
        }
    }
    if !stores.is_empty() {
        output.push_str("stores[");
        let entries: Vec<String> = stores.iter().map(ToString::to_string).collect();
        output.push_str(&entries.join(", "));
        output.push(']');
    }
}

pub fn unparse_enum_case(enum_name: &str, case: &EnumCase) -> String {
    format!("{enum_name}::{}", case.name)
}

pub fn unparse_variant_case(variant_name: &str, case: &VariantCase) -> String {
    let mut out = format!("{variant_name}::{}", case.name);
    if let Some(payload) = &case.payload {
        out.push('(');
        unparse_type_into(payload, &mut out);
        out.push(')');
    }
    out
}

pub fn unparse_struct_field(struct_name: &str, field: &StructField) -> String {
    let mut out = format!("{struct_name}.{}: ", field.name);
    unparse_type_into(&field.ty, &mut out);
    out
}

use crate::lexer::is_valid_ident;
use crate::tir::{
    TirBinaryOp, TirBlock, TirEnum, TirExpr, TirExprKind, TirFlags, TirFunction, TirGlobal,
    TirLiteralPattern, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct,
    TirUnaryOp, TypeId, TypeTable,
};

/// Unparses TIR back to pseudo-Wado source code.
/// The output shows the code after monomorphization and lowering.
/// Note: Monomorphized names like `Box<i32>` are quoted to make the output parseable.
pub struct TirUnparser<'a> {
    type_table: &'a TypeTable,
    output: String,
    indent_level: usize,
    /// When true, suppress internal debug annotations (e.g.,
    /// `@capture[0]:n` becomes `n`). Used for closure source-form
    /// rendering exposed to user-facing inspect output. Default
    /// `false` keeps the debug-friendly form for TIR dumps.
    source_form: bool,
}

impl<'a> TirUnparser<'a> {
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            output: String::new(),
            indent_level: 0,
            source_form: false,
        }
    }

    /// Enable source-form rendering: suppresses internal annotations
    /// like `@capture[i]:` so the output reflects user-written names.
    fn source_form(mut self) -> Self {
        self.source_form = true;
        self
    }

    /// Quote an identifier if it contains characters that make it invalid Wado syntax.
    /// Valid Wado identifiers match /^[a-zA-Z_][a-zA-Z0-9_]*$/
    /// `::` is allowed as a namespace separator (each segment must be a valid identifier).
    /// Names with `<`, `>`, `,` (monomorphized), `^` (trait), `/`, `.` need quoting.
    fn quote_if_needed(name: &str) -> String {
        if name.split("::").all(is_valid_ident) {
            name.to_string()
        } else {
            format!("\"{name}\"")
        }
    }

    fn emit_kw_if(&mut self, cond: bool, kw: &str) {
        if cond {
            self.output.push_str(kw);
        }
    }

    /// Emit `f(self, item)` for each item separated by `", "`.
    fn comma_sep<I, F>(&mut self, items: I, f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        self.comma_sep_with(", ", items, f);
    }

    /// Like `comma_sep`, but with a custom `separator` between items.
    fn comma_sep_with<I, F>(&mut self, separator: &str, items: I, mut f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                self.output.push_str(separator);
            }
            f(self, item);
        }
    }

    /// Emit `open`, comma-separated items, then `close`.
    fn delimited<I, F>(&mut self, open: &str, close: &str, items: I, f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        self.output.push_str(open);
        self.comma_sep(items, f);
        self.output.push_str(close);
    }

    /// Emit a turbofish `::<T1, T2, ...>` for a list of monomorphized type ids.
    fn unparse_type_args(&mut self, args: &[crate::tir::TypeId]) {
        if args.is_empty() {
            return;
        }
        self.delimited("::<", ">", args, |s, t| {
            let name = s.type_table.type_name(*t);
            s.output.push_str(&name);
        });
    }

    /// Emit a `{ ... body ... }` block at one extra indent level.
    fn emit_indented_block<F>(&mut self, body: F)
    where
        F: FnOnce(&mut Self),
    {
        self.output.push_str(" {\n");
        self.indent_level += 1;
        body(self);
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    pub fn unparse(mut self, module: &TirModule) -> String {
        self.unparse_module(module);
        self.output
    }

    fn unparse_module(&mut self, module: &TirModule) {
        if !module.imports.is_empty() {
            self.output.push_str("// Imports\n");
            for import in &module.imports {
                self.output.push_str("// ");
                self.output.push_str(&import.namespace);
                self.output.push_str("::");
                self.output.push_str(&import.canonical_name);
                self.output.push('\n');
            }
            self.output.push('\n');
        }

        for g in &module.globals {
            self.unparse_tir_global(g);
            self.output.push('\n');
        }

        for s in &module.structs {
            self.unparse_struct(s);
            self.output.push('\n');
        }

        for e in &module.enums {
            self.unparse_enum(e);
            self.output.push('\n');
        }

        for f in &module.flags {
            self.unparse_flags_tir(f);
            self.output.push('\n');
        }

        for f_rc in &module.functions {
            let f = f_rc.borrow();
            self.unparse_function(&f);
            self.output.push('\n');
        }

        if let Some(data) = &module.data_section {
            self.output.push_str("__DATA__\n");
            self.output.push_str(data);
        }
    }

    fn unparse_tir_global(&mut self, g: &TirGlobal) {
        self.write_indent();
        self.emit_kw_if(g.is_pub, "pub ");
        self.output.push_str("global ");
        self.emit_kw_if(g.mutable, "mut ");
        self.output.push_str(&g.name);
        self.output.push_str(": ");
        self.output.push_str(&self.type_table.type_name(g.ty));
        self.output.push_str(" = ");
        self.unparse_expr(&g.initializer);
        self.output.push_str(";\n");
    }

    fn unparse_struct(&mut self, s: &TirStruct) {
        self.write_indent();
        self.emit_kw_if(s.is_pub, "pub ");
        self.output.push_str("struct ");
        self.output.push_str(&Self::quote_if_needed(&s.name));

        // Generic params (only present for unmonomorphized structs).
        if !s.type_params.is_empty() {
            self.delimited("<", ">", &s.type_params, |s, param| {
                s.output.push_str(&param.name);
                if !param.bounds.is_empty() {
                    s.output.push_str(": ");
                    s.output.push_str(&param.bounds.join(" + "));
                }
                if let Some(default_type) = param.default {
                    s.output.push_str(" = ");
                    let name = s.type_table.type_name(default_type);
                    s.output.push_str(&name);
                }
            });
        }

        self.emit_indented_block(|this| {
            for field in &s.fields {
                this.write_indent();
                this.emit_kw_if(field.is_pub, "pub ");
                this.output.push_str(&field.name);
                this.output.push_str(": ");
                this.output
                    .push_str(&this.type_table.type_name(field.type_id));
                this.output.push_str(",\n");
            }
        });
        self.output.push('\n');
    }

    fn unparse_enum(&mut self, e: &TirEnum) {
        self.write_indent();
        self.emit_kw_if(e.is_pub, "pub ");
        self.output.push_str("enum ");
        self.output.push_str(&e.name);
        self.emit_indented_block(|this| {
            for case in &e.cases {
                this.write_indent();
                this.output.push_str(&case.name);
                // Enum cases have no payload (unlike variant cases)
                this.output.push_str(",\n");
            }
        });
        self.output.push('\n');
    }

    fn unparse_flags_tir(&mut self, f: &TirFlags) {
        self.write_indent();
        self.emit_kw_if(f.is_pub, "pub ");
        self.output.push_str("flags ");
        self.output.push_str(&f.name);
        self.emit_indented_block(|this| {
            for member in &f.members {
                this.write_indent();
                this.output.push_str(&member.name);
                this.output.push_str(",  // 0x");
                this.output.push_str(&format!("{:x}", member.bitmask));
                this.output.push('\n');
            }
        });
        self.output.push('\n');
    }

    fn unparse_function(&mut self, f: &TirFunction) {
        if let Some(attr) = inline_hint_attr(f.inline_hint) {
            self.write_indent();
            self.output.push_str(attr);
            self.output.push('\n');
        }
        self.write_indent();
        self.emit_kw_if(f.is_pub, "pub ");
        self.emit_kw_if(f.is_export, "export ");
        self.output.push_str("fn ");
        self.output.push_str(&Self::quote_if_needed(&f.name));

        // Generic params (only present for unmonomorphized functions).
        if !f.type_params.is_empty() {
            self.delimited("<", ">", &f.type_params, |s, param| {
                s.output.push_str(&param.name);
                if !param.bounds.is_empty() {
                    s.output.push_str(": ");
                    s.output.push_str(&param.bounds.join(" + "));
                }
                if let Some(default_type) = param.default {
                    s.output.push_str(" = ");
                    let name = s.type_table.type_name(default_type);
                    s.output.push_str(&name);
                }
            });
        }

        self.delimited("(", ")", &f.params, TirUnparser::unparse_param);

        if f.return_type != TypeTable::UNIT {
            self.output.push_str(" -> ");
            self.output
                .push_str(&self.type_table.type_name(f.return_type));
        }

        self.unparse_tir_with_clause(&f.effects, &f.stores);

        if let Some(body) = &f.body {
            self.emit_indented_block(|this| this.unparse_block(body));
            self.output.push('\n');
        } else {
            self.output.push_str(";\n");
        }
    }

    fn unparse_tir_with_clause(&mut self, effects: &[super::tir::EffectRef], stores: &[String]) {
        if effects.is_empty() && stores.is_empty() {
            return;
        }
        self.output.push_str(" with ");
        if !effects.is_empty() {
            self.comma_sep(effects, |s, e| s.output.push_str(e.name()));
            if !stores.is_empty() {
                self.output.push_str(", ");
            }
        }
        if !stores.is_empty() {
            self.output.push_str("stores[");
            self.output.push_str(&stores.join(", "));
            self.output.push(']');
        }
    }

    fn unparse_param(&mut self, param: &TirParam) {
        self.output.push_str(&param.name);
        self.output.push_str(": ");
        self.output
            .push_str(&self.type_table.type_name(param.type_id));
    }

    fn unparse_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.unparse_stmt(stmt);
        }
    }

    fn unparse_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                name,
                is_mut,
                is_reactive,
                type_id,
                value,
                ..
            } => {
                self.write_indent();
                self.output.push_str("let ");
                if *is_reactive {
                    self.output.push_str("reactive ");
                }
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.output.push_str(name);
                self.output.push_str(": ");
                self.output.push_str(&self.type_table.type_name(*type_id));
                self.output.push_str(" = ");
                self.unparse_expr(value);
                self.output.push_str(";\n");
            }
            TirStmtKind::Expr(expr) => {
                self.write_indent();
                self.unparse_expr(expr);
                self.output.push_str(";\n");
            }
            TirStmtKind::Return { value } => {
                self.write_indent();
                self.output.push_str("return");
                if let Some(v) = value {
                    self.output.push(' ');
                    self.unparse_expr(v);
                }
                self.output.push_str(";\n");
            }
            TirStmtKind::TaskReturn { value } => {
                self.write_indent();
                self.output.push_str("task return ");
                self.unparse_expr(value);
                self.output.push_str(";\n");
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.write_indent();
                self.output.push_str("if ");
                self.unparse_expr(condition);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                self.unparse_block(then_block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
                if let Some(else_blk) = else_block {
                    self.output.push_str(" else {\n");
                    self.indent_level += 1;
                    self.unparse_block(else_blk);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push('}');
                }
                self.output.push('\n');
            }
            TirStmtKind::Loop { body } => {
                self.write_indent();
                self.output.push_str("loop {\n");
                self.indent_level += 1;
                self.unparse_block(body);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            TirStmtKind::Break { label, value } => {
                self.write_indent();
                self.output.push_str("break");
                if let Some(lbl) = label {
                    self.output.push(' ');
                    self.output.push_str(lbl);
                    if let Some(val) = value {
                        self.output.push_str(": ");
                        self.unparse_expr(val);
                    }
                }
                self.output.push_str(";\n");
            }
            TirStmtKind::Continue => {
                self.write_indent();
                self.output.push_str("continue;\n");
            }
            TirStmtKind::LabeledBlock { label, block } => {
                self.write_indent();
                self.output.push_str(label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            TirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => {
                self.write_indent();
                self.output.push_str("let ");
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.unparse_tir_pattern(pattern);
                self.output.push_str(" = ");
                self.unparse_expr(value);
                self.output.push_str(";\n");
            }
            TirStmtKind::VariadicForOf {
                iterable,
                binding_name,
                is_mut,
                body,
                ..
            } => {
                self.write_indent();
                self.output.push_str("for let ");
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.output.push_str(binding_name);
                self.output.push_str(" of <variadic> ");
                self.unparse_expr(iterable);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                for stmt in &body.stmts {
                    self.unparse_stmt(stmt);
                }
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
        }
    }

    fn unparse_tir_pattern(&mut self, pattern: &TirPattern) {
        match pattern {
            TirPattern::Wildcard => self.output.push('_'),
            TirPattern::Binding { name, .. } => self.output.push_str(name),
            TirPattern::Literal(lit) => emit_tir_literal_pattern(lit, &mut self.output),
            TirPattern::Tuple(patterns, has_rest) => {
                self.output.push('[');
                self.comma_sep(patterns, TirUnparser::unparse_tir_pattern);
                if *has_rest {
                    if !patterns.is_empty() {
                        self.output.push_str(", ");
                    }
                    self.output.push_str("..");
                }
                self.output.push(']');
            }
            TirPattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    self.delimited("(", ")", bindings, TirUnparser::unparse_tir_pattern);
                }
            }
            TirPattern::Enum { case_name, .. } => self.output.push_str(case_name),
            TirPattern::Struct { fields, .. } => {
                self.output.push_str("{ ");
                self.comma_sep(fields, |s, field| {
                    s.output.push_str(&field.field_name);
                    if !matches!(&field.pattern, TirPattern::Binding { name, .. } if name == &field.field_name)
                    {
                        s.output.push_str(": ");
                        s.unparse_tir_pattern(&field.pattern);
                    }
                });
                self.output.push_str(" }");
            }
            TirPattern::Or(alternatives) => {
                self.comma_sep_with(" | ", alternatives, TirUnparser::unparse_tir_pattern);
            }
            TirPattern::ConstantValue { expr } => self.unparse_expr(expr),
            TirPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                self.output.push_str(&start.to_string());
                self.output.push_str(if *inclusive { "..=" } else { "..<" });
                self.output.push_str(&end.to_string());
            }
        }
    }

    fn unparse_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::IntLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            TirExprKind::FloatLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            TirExprKind::BoolLiteral(b) => {
                self.output.push_str(if *b { "true" } else { "false" });
            }
            TirExprKind::CharLiteral(c) => {
                self.output.push('\'');
                self.output.push_str(&escape_char(*c));
                self.output.push('\'');
            }
            TirExprKind::StringLiteral(s) => {
                self.output.push('"');
                self.output.push_str(&escape_string(s));
                self.output.push('"');
            }
            TirExprKind::BytesLiteral(bytes) => {
                self.output
                    .push_str(&format!("#include_bytes(/* {} bytes */)", bytes.len()));
            }
            TirExprKind::Null => {
                self.output.push_str("null");
            }
            TirExprKind::VariantConstruct {
                case_name, payload, ..
            } => {
                // Get the variant type name from the type_id
                let type_name = self.type_table.type_name(expr.type_id);
                self.output.push_str(&type_name);
                self.output.push_str("::");
                self.output.push_str(case_name);
                if let Some(payload_expr) = payload {
                    self.output.push('(');
                    self.unparse_expr(payload_expr);
                    self.output.push(')');
                }
            }
            TirExprKind::EnumConstruct { case_name, .. } => {
                // Get the enum type name from the type_id
                let type_name = self.type_table.type_name(expr.type_id);
                self.output.push_str(&type_name);
                self.output.push_str("::");
                self.output.push_str(case_name);
            }
            TirExprKind::Unit => {
                self.output.push_str("()");
            }
            TirExprKind::Local { name, .. } => {
                self.output.push_str(name);
            }
            TirExprKind::FuncRef {
                name,
                module_source,
                type_args,
            } => {
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
                if !type_args.is_empty() {
                    self.output.push_str("::<");
                    for (i, ty) in type_args.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        let name = self.type_table.type_name(*ty);
                        self.output.push_str(&name);
                    }
                    self.output.push('>');
                }
            }
            TirExprKind::GlobalVarGet {
                name,
                module_source,
            } => {
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
            }
            TirExprKind::GlobalVarSet {
                name,
                module_source,
                value,
            } => {
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
                self.output.push_str(" = ");
                self.unparse_expr(value);
            }
            TirExprKind::Capture { name, index } => {
                if self.source_form {
                    // Source-form: just the original name. The presence of
                    // a captures[...] header (rendered separately) already
                    // signals that the closure depends on captured locals.
                    self.output.push_str(name);
                } else {
                    // Debug-form: include capture index for TIR dumps.
                    self.output.push_str(&format!("@capture[{index}]:{name}"));
                }
            }
            TirExprKind::Binary { left, op, right } => {
                self.output.push('(');
                self.unparse_expr(left);
                self.output.push(' ');
                self.output.push_str(tir_binary_op_str(*op));
                self.output.push(' ');
                self.unparse_expr(right);
                self.output.push(')');
            }
            TirExprKind::Unary { op, expr: inner } => {
                self.output.push_str(tir_unary_op_str(*op));
                self.unparse_expr(inner);
            }
            TirExprKind::Assign { target, value } => {
                self.unparse_expr(target);
                self.output.push_str(" = ");
                self.unparse_expr(value);
            }
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                self.unparse_expr(inner);
                self.output.push_str(" as ");
                self.output
                    .push_str(&self.type_table.type_name(*target_type));
            }
            TirExprKind::Call {
                func,
                type_args,
                args,
                ..
            } => {
                let func_name = func.name.clone();
                let full_name = if func.module_source.clone().is_entry_point() {
                    func_name
                } else {
                    let module_path = func.module_path();
                    format!("{}::{func_name}", module_path.join("::"))
                };
                self.output.push_str(&Self::quote_if_needed(&full_name));
                self.unparse_type_args(type_args);
                self.delimited("(", ")", args, |s, arg| s.unparse_expr(&arg.expr));
            }
            TirExprKind::CmRawCall { local_name, args } => {
                self.output.push_str("cm_raw_call ");
                self.output.push_str(local_name);
                self.delimited("(", ")", args, TirUnparser::unparse_expr);
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => {
                // The elaborator wraps `self` receivers in `&`/`&mut` automatically;
                // strip that wrapper so the rendering reflects the source value.
                let actual_receiver = match &receiver.kind {
                    TirExprKind::Unary {
                        op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                        expr: inner,
                    } => inner.as_ref(),
                    _ => receiver.as_ref(),
                };
                self.unparse_expr(actual_receiver);
                self.output.push('.');
                // Quote the full resolved method name (e.g. `"Type::method"`) so
                // the output captures which impl was selected.
                self.output.push_str(&Self::quote_if_needed(&func.name));
                self.unparse_type_args(type_args);
                self.delimited("(", ")", args, |s, arg| s.unparse_expr(&arg.expr));
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                self.unparse_expr(inner);
                self.output.push('.');
                self.output.push_str(field_name);
            }
            TirExprKind::Index { expr: array, index } => {
                self.unparse_expr(array);
                self.output.push('[');
                self.unparse_expr(index);
                self.output.push(']');
            }
            TirExprKind::Block(block) => {
                self.output.push_str("{\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.output.push_str("if ");
                self.unparse_expr(condition);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                self.unparse_block(then_branch);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
                if let Some(else_blk) = else_branch {
                    self.output.push_str(" else {\n");
                    self.indent_level += 1;
                    self.unparse_block(else_blk);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push('}');
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.output.push_str("match ");
                self.unparse_expr(scrutinee);
                self.emit_indented_block(|this| {
                    for arm in arms {
                        this.write_indent();
                        this.unparse_tir_pattern(&arm.pattern);
                        if let Some(guard) = &arm.guard {
                            this.output.push_str(" && ");
                            this.unparse_expr(guard);
                        }
                        this.output.push_str(" => ");
                        this.unparse_expr(&arm.body);
                        this.output.push_str(",\n");
                    }
                });
            }
            TirExprKind::StructLiteral {
                struct_name,
                fields,
                ..
            } => {
                // Functor structs are rendered as `&Name { ... }` to mirror the
                // reference type that the elaborator attached.
                if matches!(
                    self.type_table.get(expr.type_id),
                    crate::tir::ResolvedType::Ref(_)
                ) {
                    self.output.push('&');
                }
                self.output.push_str(struct_name);
                self.output.push_str(" { ");
                self.comma_sep(fields, |s, field| {
                    s.output.push_str(&field.name);
                    s.output.push_str(": ");
                    s.unparse_expr(&field.value);
                });
                self.output.push_str(" }");
            }
            TirExprKind::TupleLiteral { elements } => {
                self.delimited("[", "]", elements, TirUnparser::unparse_expr);
            }
            TirExprKind::TupleSpread { expr } | TirExprKind::TupleZip { expr } => {
                self.output.push_str("[..");
                self.unparse_expr(expr);
                self.output.push(']');
            }
            TirExprKind::TypePackExpansion { call_expr, .. } => {
                self.output.push_str("[..");
                self.unparse_expr(call_expr);
                self.output.push(']');
            }
            TirExprKind::Closure {
                params,
                body,
                captures,
                ..
            } => {
                self.unparse_closure_form(params, captures, body);
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.unparse_expr(callee);
                self.delimited("(", ")", args, TirUnparser::unparse_expr);
            }
            TirExprKind::LabeledBlock { label, block, .. } => {
                self.output.push_str(label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }

            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr } => {
                self.output.push_str("__variant_tag(");
                self.unparse_expr(expr);
                self.output.push(')');
            }
            TirExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => {
                self.output.push_str("__variant_test(");
                self.unparse_expr(expr);
                self.output
                    .push_str(&format!(", case={case_index}, name={case_name})"));
            }
            TirExprKind::VariantPayload {
                expr, case_index, ..
            } => {
                self.output.push_str("__variant_payload(");
                self.unparse_expr(expr);
                self.output.push_str(&format!(", case={case_index})"));
            }
            TirExprKind::TemplateString { parts } => {
                self.output.push('`');
                for part in parts {
                    match part {
                        crate::tir::TirTemplatePart::Literal(s) => {
                            self.output.push_str(s);
                        }
                        crate::tir::TirTemplatePart::Interpolation {
                            expr: inner,
                            format_spec,
                        } => {
                            self.output.push('{');
                            self.unparse_expr(inner);
                            if let Some(spec) = format_spec {
                                self.output.push(':');
                                self.output.push_str(&format_spec_to_string(spec));
                            }
                            self.output.push('}');
                        }
                    }
                }
                self.output.push('`');
            }
            TirExprKind::WithHandler { bindings, body, .. } => {
                self.output.push_str("with ");
                self.comma_sep(bindings, |s, binding| {
                    if let Some(eff) = &binding.effect {
                        s.output.push_str(eff.name());
                        s.output.push_str(" => ");
                    }
                    s.unparse_expr(&binding.handler);
                });
                self.output.push_str(" do ");
                self.unparse_block(body);
            }
            TirExprKind::Resume { value } => {
                self.output.push_str("resume ");
                self.unparse_expr(value);
            }
        }
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("    ");
        }
    }

    /// Emit the closure literal form `|name: Type, ...| body` (with an
    /// optional ` captures[...]` clause) into the unparser's output.
    /// Shared by the `TirExprKind::Closure` arm and by
    /// [`unparse_tir_closure_source`].
    fn unparse_closure_form(
        &mut self,
        params: &[(String, TypeId)],
        captures: &[crate::tir::TirCapture],
        body: &TirExpr,
    ) {
        self.delimited("|", "|", params, |s, (name, type_id)| {
            s.output.push_str(name);
            s.output.push_str(": ");
            let ty = s.type_table.type_name(*type_id);
            s.output.push_str(&ty);
        });
        if !captures.is_empty() {
            self.output.push_str(" captures");
            self.delimited("[", "]", captures, |s, cap| s.output.push_str(&cap.name));
        }
        self.output.push(' ');
        self.unparse_expr(body);
    }
}

fn emit_tir_literal_pattern(lit: &TirLiteralPattern, output: &mut String) {
    match lit {
        TirLiteralPattern::I128(v) => output.push_str(&v.to_string()),
        TirLiteralPattern::U128(v) => output.push_str(&v.to_string()),
        TirLiteralPattern::Bool(b) => output.push_str(if *b { "true" } else { "false" }),
        TirLiteralPattern::Char(c) => {
            output.push('\'');
            output.push(*c);
            output.push('\'');
        }
        TirLiteralPattern::String(s) => {
            output.push('"');
            output.push_str(s);
            output.push('"');
        }
        TirLiteralPattern::Null => output.push_str("null"),
    }
}

/// Map a TIR inline hint to its `#[inline...]` attribute, or `None` for the
/// default (no attribute).
fn inline_hint_attr(hint: crate::tir::InlineHint) -> Option<&'static str> {
    match hint {
        crate::tir::InlineHint::Auto => None,
        crate::tir::InlineHint::Hint => Some("#[inline]"),
        crate::tir::InlineHint::Always => Some("#[inline(always)]"),
        crate::tir::InlineHint::Never => Some("#[inline(never)]"),
    }
}

fn tir_binary_op_str(op: TirBinaryOp) -> &'static str {
    match op {
        TirBinaryOp::Add => "+",
        TirBinaryOp::Sub => "-",
        TirBinaryOp::Mul => "*",
        TirBinaryOp::Div => "/",
        TirBinaryOp::Mod => "%",
        TirBinaryOp::Eq => "==",
        TirBinaryOp::NotEq => "!=",
        TirBinaryOp::Lt => "<",
        TirBinaryOp::LtEq => "<=",
        TirBinaryOp::Gt => ">",
        TirBinaryOp::GtEq => ">=",
        TirBinaryOp::And => "&&",
        TirBinaryOp::Or => "||",
        TirBinaryOp::BitAnd => "&",
        TirBinaryOp::BitOr => "|",
        TirBinaryOp::BitXor => "^",
        TirBinaryOp::Shl => "<<",
        TirBinaryOp::Shr => ">>",
        TirBinaryOp::RefEq => "ref.eq",
        TirBinaryOp::RefNotEq => "ref.ne",
    }
}

fn tir_unary_op_str(op: TirUnaryOp) -> &'static str {
    match op {
        TirUnaryOp::Neg => "-",
        TirUnaryOp::Not => "!",
        TirUnaryOp::BitNot => "~",
        TirUnaryOp::Ref => "&",
        TirUnaryOp::MutRef => "&mut ",
        TirUnaryOp::Deref => "*",
    }
}

/// Unparse a TIR closure as `|name: Type, ...| body` (or `|name: Type, ...|
/// captures[...] body` when the closure captures locals) source text.
///
/// Used by `lower::plan::closure` to bake the per-literal source string into
/// `__Closure_N^InspectAlt::inspect_alt` without requiring every TIR
/// `Closure` node to carry an unparsed-AST string.
///
/// The `captures[name1, name2, ...]` clause has no surface-syntax
/// counterpart in Wado (closures capture implicitly), but is shown in
/// the `:#?` debug output by design: it makes captured-environment
/// dependencies visible at inspect time, which is the whole point of
/// pretty-printing a closure in the first place. Non-capturing closures
/// produce output that round-trips through the parser; capturing
/// closures intentionally do not.
pub fn unparse_tir_closure_source(
    params: &[(String, TypeId)],
    captures: &[crate::tir::TirCapture],
    body: &TirExpr,
    type_table: &TypeTable,
) -> String {
    let mut unparser = TirUnparser::new(type_table).source_form();
    unparser.unparse_closure_form(params, captures, body);
    unparser.output
}

/// Public function to unparse TIR module to pseudo-Wado source
pub fn unparse_tir(module: &TirModule) -> String {
    let type_table_ref = module.type_table.borrow();
    let unparser = TirUnparser::new(&type_table_ref);
    unparser.unparse(module)
}

/// Unparse a `FlatPackage` (flat TIR lists) to pseudo-Wado source
pub fn unparse_flat_package(package: &crate::flat_package::FlatPackage) -> String {
    let type_table_ref = package.type_table.borrow();
    let mut unparser = TirUnparser::new(&type_table_ref);

    // Imports
    if !package.imports.is_empty() {
        unparser.output.push_str("// Imports\n");
        for import in &package.imports {
            unparser.output.push_str("// ");
            unparser.output.push_str(&import.namespace);
            unparser.output.push_str("::");
            unparser.output.push_str(&import.canonical_name);
            unparser.output.push('\n');
        }
        unparser.output.push('\n');
    }

    // Globals
    for g in &package.globals {
        unparser.unparse_tir_global(g);
        unparser.output.push('\n');
    }

    // Structs
    for s in &package.structs {
        unparser.unparse_struct(s);
        unparser.output.push('\n');
    }

    // Enums
    for e in &package.enums {
        unparser.unparse_enum(e);
        unparser.output.push('\n');
    }

    // Flags
    for f in &package.flags {
        unparser.unparse_flags_tir(f);
        unparser.output.push('\n');
    }

    // Functions
    for f_rc in &package.functions {
        let f = f_rc.borrow();
        unparser.unparse_function(&f);
        unparser.output.push('\n');
    }

    unparser.output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_string() {
        assert_eq!(escape_string("hello"), "hello");
        assert_eq!(escape_string("hello\nworld"), "hello\\nworld");
        assert_eq!(escape_string("say \"hi\""), "say \\\"hi\\\"");
    }

    #[test]
    fn test_binary_op_str() {
        assert_eq!(binary_op_str(BinaryOp::Add), "+");
        assert_eq!(binary_op_str(BinaryOp::Eq), "==");
        assert_eq!(binary_op_str(BinaryOp::And), "&&");
    }

    #[test]
    fn test_compound_op_str() {
        assert_eq!(compound_op_str(CompoundAssignOp::Add), "+=");
        assert_eq!(compound_op_str(CompoundAssignOp::Div), "/=");
    }
}
