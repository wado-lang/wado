//! Document highlight, powered by `wado_compiler::annotate`.
//!
//! Returns every occurrence of the symbol named at the cursor that lives
//! inside the requested document. References to the same symbol from other
//! files are filtered out — that's `textDocument/references`' job.
//!
//! Each occurrence is classified:
//! - `Write` for the declaration and for use-sites that are direct targets of
//!   `=`, `+=`, etc.
//! - `Read` for every other use-site.

use wado_compiler::CompilerHost;
use wado_compiler::annotate::{Annotated, annotate};
use wado_compiler::ast::{
    self, AssignExpr, AstId, Block, CompoundAssignExpr, Condition, ConditionElement, Expr, Item,
    MatchExpr, Module, Stmt,
};
use wado_compiler::name::ModuleSource;

use crate::diagnostics::{Position, Range};
use crate::location::{resolve_def_key, span_to_range, uri_to_filename};

/// LSP `DocumentHighlightKind` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightKind {
    Text = 1,
    Read = 2,
    Write = 3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentHighlight {
    pub range: Range,
    pub kind: HighlightKind,
}

pub async fn document_highlight<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    host: &H,
) -> Vec<DocumentHighlight> {
    let filename = uri_to_filename(uri);
    let Ok(annotated) = annotate(source, host, Some(&filename)).await else {
        return Vec::new();
    };

    let module = annotated.entry_module_source.clone();
    let line = position.line as usize + 1;
    let col = position.character as usize + 1;

    let Some(def_key) = resolve_def_key(&annotated, &module, line, col) else {
        return Vec::new();
    };

    let write_targets = collect_write_target_ids(&annotated, &module);
    let mut out = Vec::new();

    if def_key.module == module
        && let Some(span) = annotated
            .name_span_of(&def_key)
            .or_else(|| annotated.symbol_at(&def_key).and_then(|s| s.span))
    {
        out.push(DocumentHighlight {
            range: span_to_range(&span),
            kind: HighlightKind::Write,
        });
    }

    for use_key in annotated.references_to(&def_key) {
        if use_key.module != module {
            continue;
        }
        let Some(span) = annotated.span_of_key(&use_key) else {
            continue;
        };
        let kind = if write_targets.contains(&use_key.ast_id) {
            HighlightKind::Write
        } else {
            HighlightKind::Read
        };
        out.push(DocumentHighlight {
            range: span_to_range(&span),
            kind,
        });
    }

    out.sort_by_key(|h| (h.range.start.line, h.range.start.character));
    out.dedup();
    out
}

/// Collect every `IdentExpr.id` that appears as the direct target of
/// `Expr::Assign` / `Expr::CompoundAssign` in the given module.
fn collect_write_target_ids(annotated: &Annotated, module: &ModuleSource) -> Vec<AstId> {
    let mut writes = Vec::new();
    let Some(module) = annotated.modules.get(module) else {
        return writes;
    };
    visit_module(module, &mut writes);
    writes
}

fn visit_module(module: &Module, writes: &mut Vec<AstId>) {
    for item in &module.items {
        visit_item(item, writes);
    }
}

fn visit_item(item: &Item, writes: &mut Vec<AstId>) {
    match item {
        Item::Function(f) => {
            if let Some(body) = &f.body {
                visit_block(body, writes);
            }
        }
        Item::Impl(imp) => {
            for m in &imp.methods {
                if let Some(body) = &m.body {
                    visit_block(body, writes);
                }
            }
        }
        Item::Trait(t) => {
            for m in &t.methods {
                if let Some(body) = &m.body {
                    visit_block(body, writes);
                }
            }
        }
        Item::Test(t) => visit_block(&t.body, writes),
        Item::Global(g) => visit_expr(&g.initializer, writes),
        _ => {}
    }
}

fn visit_block(block: &Block, writes: &mut Vec<AstId>) {
    for stmt in &block.stmts {
        visit_stmt(stmt, writes);
    }
}

fn visit_stmt(stmt: &Stmt, writes: &mut Vec<AstId>) {
    match stmt {
        Stmt::Let(s) => {
            if let Some(v) = &s.value {
                visit_expr(v, writes);
            }
        }
        Stmt::Expr(s) => visit_expr(&s.expr, writes),
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                visit_expr(v, writes);
            }
        }
        Stmt::TaskReturn(s) => visit_expr(&s.value, writes),
        Stmt::If(s) => {
            visit_condition(&s.condition, writes);
            visit_block(&s.then_block, writes);
            if let Some(eb) = &s.else_block {
                visit_block(eb, writes);
            }
        }
        Stmt::While(s) => {
            visit_condition(&s.condition, writes);
            visit_block(&s.body, writes);
        }
        Stmt::For(s) => {
            if let Some(init) = &s.init {
                visit_stmt(init, writes);
            }
            if let Some(cond) = &s.condition {
                visit_condition(cond, writes);
            }
            if let Some(u) = &s.update {
                visit_expr(u, writes);
            }
            visit_block(&s.body, writes);
        }
        Stmt::ForOf(s) => {
            visit_expr(&s.iterable, writes);
            visit_block(&s.body, writes);
        }
        Stmt::Loop(s) => visit_block(&s.body, writes),
        Stmt::Match(m) => visit_match(m, writes),
        Stmt::Break(s) => {
            if let Some(v) = &s.value {
                visit_expr(v, writes);
            }
        }
        Stmt::Continue(_) => {}
        Stmt::Assert(s) => {
            visit_expr(&s.condition, writes);
            if let Some(msg) = &s.message {
                visit_expr(msg, writes);
            }
        }
        Stmt::LabeledBlock(s) => visit_block(&s.block, writes),
    }
}

fn visit_condition(cond: &Condition, writes: &mut Vec<AstId>) {
    match cond {
        Condition::Expr(e) => visit_expr(e, writes),
        Condition::LetChain { elements, .. } => {
            for el in elements {
                match el {
                    ConditionElement::Let { expr, .. } => visit_expr(expr, writes),
                    ConditionElement::Expr(e) => visit_expr(e, writes),
                }
            }
        }
    }
}

fn visit_match(m: &MatchExpr, writes: &mut Vec<AstId>) {
    visit_expr(&m.expr, writes);
    for arm in &m.arms {
        if let Some(g) = &arm.guard {
            visit_expr(g, writes);
        }
        visit_expr(&arm.body, writes);
    }
}

fn visit_expr(expr: &Expr, writes: &mut Vec<AstId>) {
    match expr {
        Expr::Assign(e) => visit_assign(e, writes),
        Expr::CompoundAssign(e) => visit_compound_assign(e, writes),
        Expr::Ident(_) | Expr::Literal(_) => {}
        Expr::Binary(e) => {
            visit_expr(&e.left, writes);
            visit_expr(&e.right, writes);
        }
        Expr::Unary(e) => visit_expr(&e.expr, writes),
        Expr::ComparisonChain(e) => {
            visit_expr(&e.first, writes);
            for c in &e.comparisons {
                visit_expr(&c.right, writes);
            }
        }
        Expr::Call(e) => {
            visit_expr(&e.callee, writes);
            for a in &e.args {
                visit_expr(a, writes);
            }
        }
        Expr::MethodCall(e) => {
            visit_expr(&e.receiver, writes);
            for a in &e.args {
                visit_expr(a, writes);
            }
        }
        Expr::StaticMethodCall(e) => {
            for a in &e.args {
                visit_expr(a, writes);
            }
        }
        Expr::FieldAccess(e) => visit_expr(&e.expr, writes),
        Expr::Index(e) => {
            visit_expr(&e.expr, writes);
            visit_expr(&e.index, writes);
        }
        Expr::Block(b) => visit_block(b, writes),
        Expr::If(e) => {
            visit_condition(&e.condition, writes);
            visit_block(&e.then_block, writes);
            if let Some(eb) = &e.else_block {
                visit_block(eb, writes);
            }
        }
        Expr::Match(m) => visit_match(m, writes),
        Expr::Matches(m) => {
            visit_expr(&m.expr, writes);
            if let Some(g) = &m.guard {
                visit_expr(g, writes);
            }
        }
        Expr::Closure(c) => visit_expr(&c.body, writes),
        Expr::TemplateString(t) => {
            for part in &t.parts {
                if let ast::TemplatePart::Interpolation { expr, .. } = part {
                    visit_expr(expr, writes);
                }
            }
        }
        Expr::Cast(c) => visit_expr(&c.expr, writes),
        Expr::StructLiteral(s) => {
            for f in &s.fields {
                visit_expr(&f.value, writes);
            }
        }
        Expr::TupleLiteral(t) => {
            for el in &t.elements {
                visit_expr(el, writes);
            }
        }
        Expr::LabeledBlock(lb) => visit_block(&lb.block, writes),
        Expr::TryOp(t) => visit_expr(&t.expr, writes),
        Expr::Spread(inner, _) => visit_expr(inner, writes),
        Expr::Range(r) => {
            visit_expr(&r.start, writes);
            visit_expr(&r.end, writes);
        }
    }
}

fn visit_assign(e: &AssignExpr, writes: &mut Vec<AstId>) {
    record_target(&e.target, writes);
    visit_expr(&e.target, writes);
    visit_expr(&e.value, writes);
}

fn visit_compound_assign(e: &CompoundAssignExpr, writes: &mut Vec<AstId>) {
    record_target(&e.target, writes);
    visit_expr(&e.target, writes);
    visit_expr(&e.value, writes);
}

/// Record an `IdentExpr` target as a write site. Only direct identifier
/// targets count — for `obj.field = v`, the write is on `field`, not `obj`,
/// so the `IdentExpr` for `obj` remains a Read.
fn record_target(target: &Expr, writes: &mut Vec<AstId>) {
    if let Expr::Ident(id) = target {
        writes.push(id.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use wado_compiler::{Diagnostic as CompilerDiagnostic, SourceError};

    struct TestHost {
        sources: IndexMap<String, Vec<u8>>,
    }

    impl TestHost {
        fn single(path: &str, source: &str) -> Self {
            let mut sources = IndexMap::new();
            sources.insert(path.to_string(), source.as_bytes().to_vec());
            Self { sources }
        }
    }

    impl CompilerHost for TestHost {
        async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
            self.sources
                .get(path)
                .cloned()
                .ok_or_else(|| SourceError::NotFound {
                    path: path.to_string(),
                })
        }

        fn emit_diagnostic(&self, _diagnostic: CompilerDiagnostic) {}
    }

    async fn highlights_at(source: &str, line: u32, character: u32) -> Vec<DocumentHighlight> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = TestHost::single(path, source);
        document_highlight(source, Position { line, character }, &uri, &host).await
    }

    fn summarize(refs: &[DocumentHighlight]) -> Vec<(u32, u32, HighlightKind)> {
        refs.iter()
            .map(|h| (h.range.start.line, h.range.start.character, h.kind))
            .collect()
    }

    #[tokio::test]
    async fn read_only_uses() {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x + x;\n}\n";
        let hl = highlights_at(source, 1, 8).await;
        assert_eq!(
            summarize(&hl),
            vec![
                (1, 8, HighlightKind::Write),
                (2, 11, HighlightKind::Read),
                (2, 15, HighlightKind::Read),
            ]
        );
    }

    #[tokio::test]
    async fn assignment_marks_target_as_write() {
        let source = "fn f() {\n    let mut x: i32 = 0;\n    x = 1;\n    x += 2;\n    let y = x;\n}\n";
        let hl = highlights_at(source, 1, 12).await;
        assert_eq!(
            summarize(&hl),
            vec![
                (1, 12, HighlightKind::Write),
                (2, 4, HighlightKind::Write),
                (3, 4, HighlightKind::Write),
                (4, 12, HighlightKind::Read),
            ]
        );
    }

    #[tokio::test]
    async fn cursor_on_use_site_works() {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x + x;\n}\n";
        let hl = highlights_at(source, 2, 11).await;
        assert_eq!(
            summarize(&hl),
            vec![
                (1, 8, HighlightKind::Write),
                (2, 11, HighlightKind::Read),
                (2, 15, HighlightKind::Read),
            ]
        );
    }

    #[tokio::test]
    async fn function_highlights_within_file() {
        let source = "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    return helper() + helper();\n}\n";
        let hl = highlights_at(source, 0, 4).await;
        assert_eq!(
            summarize(&hl),
            vec![
                (0, 3, HighlightKind::Write),
                (4, 11, HighlightKind::Read),
                (4, 22, HighlightKind::Read),
            ]
        );
    }
}
