//! AST→TIR lowering for `assert` statements.
//!
//! `assert cond[, msg];` becomes (conceptually):
//! ```text
//! {
//!     let __v0 = <intermediate0>;
//!     let __v1 = <intermediate1>;
//!     ...
//!     let __cond = <reconstructed condition>;
//!     if !__cond {
//!         panic(`Assertion failed in {#function} at {#file}:{#line}[: msg]
//! condition: <source>
//! <intermediate0_source>: {__v0:?}
//! ...`);
//!     }
//! }
//! ```
//!
//! Each captured sub-expression is bound once so that side-effecting sub-terms
//! evaluate exactly once even when they appear multiple times in `cond` or in
//! enclosing expressions.
//!
//! Implementation: the capture-and-rewrite pass is driven by [`AstFolder`].
//! `default_fold_expr` handles structural recursion through every `Expr`
//! variant, so adding a new variant to the AST is a compile-error rather than
//! a silent miss in the assert lowering.

use crate::ast::{
    self, AssertStmt, AstId, Block, CallExpr, Condition, Expr, ExprStmt, FormatSpec, IdentExpr,
    IfStmt, LabeledBlockStmt, LetStmt, Literal, LiteralExpr, Pattern, TemplatePart,
    TemplateStringExpr, UnaryExpr, UnaryOp,
};
use crate::ast_folder::{AstFolder, default_fold_expr};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::TirStmt;
use crate::unparse::unparse_expr_simple;

use super::Resolver;
use super::types::FunctionContext;

impl<H: CompilerHost> Resolver<'_, H> {
    /// Lower an `assert` AST statement to a sequence of TIR statements.
    ///
    /// The AST `assert` is preserved verbatim upstream (parser → bind →
    /// annotate); the expansion only happens here at TIR-lowering time so that
    /// LSP queries see the user's `assert cond, msg;` as-written.
    pub(super) fn lower_assert(
        &mut self,
        assert_stmt: &AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let span = assert_stmt.span;
        let parent_id = assert_stmt.id;

        // Power-assert capture: rewrite the condition so each interesting
        // sub-expression is replaced by a fresh `__vK` identifier, with a
        // matching `(source_text, expr)` pair recorded for later let binding.
        let mut capture = CaptureFolder::new(parent_id);
        let rewritten_condition = capture.fold_condition_expr(assert_stmt.condition.clone());
        let captures = capture.finish();

        // Build the synthetic block body (let bindings + check + panic).
        let mut stmts: Vec<ast::Stmt> = Vec::with_capacity(captures.len() + 2);

        for cap in &captures {
            stmts.push(ast::Stmt::Let(LetStmt {
                id: parent_id,
                pattern: Pattern::Ident {
                    id: parent_id,
                    name: cap.name.clone(),
                    span,
                },
                name_span: span,
                is_mut: false,
                is_reactive: false,
                ty: None,
                value: Some(cap.value.clone()),
                span,
            }));
        }

        let cond_name = "__cond".to_string();
        stmts.push(ast::Stmt::Let(LetStmt {
            id: parent_id,
            pattern: Pattern::Ident {
                id: parent_id,
                name: cond_name.clone(),
                span,
            },
            name_span: span,
            is_mut: false,
            is_reactive: false,
            ty: None,
            value: Some(rewritten_condition),
            span,
        }));

        let panic_message = build_panic_message(assert_stmt, &captures, parent_id, span);
        let panic_call = Expr::Call(Box::new(CallExpr {
            id: parent_id,
            callee: Expr::Ident(IdentExpr {
                id: parent_id,
                name: "panic".to_string(),
                segments: Vec::new(),
                type_args: Vec::new(),
                span,
            }),
            type_args: Vec::new(),
            args: vec![panic_message],
            has_trailing_comma: false,
            span,
        }));

        let if_stmt = ast::Stmt::If(IfStmt {
            id: parent_id,
            condition: Condition::Expr(Expr::Unary(Box::new(UnaryExpr {
                id: parent_id,
                op: UnaryOp::Not,
                expr: Expr::Ident(IdentExpr {
                    id: parent_id,
                    name: cond_name,
                    segments: Vec::new(),
                    type_args: Vec::new(),
                    span,
                }),
                span,
            }))),
            then_block: Block {
                id: parent_id,
                stmts: vec![ast::Stmt::Expr(ExprStmt {
                    id: parent_id,
                    expr: panic_call,
                    span,
                })],
                span,
            },
            else_block: None,
            span,
        });
        stmts.push(if_stmt);

        // Wrap in a labeled block so the synthetic locals are scoped to this
        // expansion. The label name mirrors the historical desugar output so
        // any external diagnostic that referenced `__assert_N` is unaffected.
        let labeled = ast::Stmt::LabeledBlock(LabeledBlockStmt {
            id: parent_id,
            // Label uses the assert's `AstId` so it is unique across all
            // asserts in the same function without needing a separate counter.
            label: format!("__assert_{}", parent_id.0),
            block: Block {
                id: parent_id,
                stmts,
                span,
            },
            span,
        });

        // Resolve the synthetic wrapper through the standard statement path.
        // The expansion is a single TirStmt (a labeled block); returning it as
        // a one-element vec keeps `resolve_stmt`'s `Vec<TirStmt>` shape.
        self.resolve_stmt(&labeled, ctx)
    }
}

/// One sub-expression captured during the power-assert walk.
struct Capture {
    /// Variable name (`__v0`, `__v1`, …) the rewritten condition refers to.
    name: String,
    /// Source text of the original sub-expression, used in the failure message.
    source: String,
    /// The (already-rewritten) expression to bind. Children already point at
    /// earlier `__vK`s so the binding evaluates each sub-term exactly once.
    value: Expr,
}

/// `AstFolder` that rewrites a condition expression in place, collecting
/// every interesting sub-expression as a `Capture` and replacing it with a
/// reference to a freshly-named `__vK` local.
struct CaptureFolder {
    parent_id: AstId,
    captures: Vec<Capture>,
    /// `source_text -> capture index` for dedup. Two structurally identical
    /// sub-expressions share one `__vK` so the failure message stays terse
    /// and we do not re-evaluate identical sub-terms.
    seen: IndexMap<String, usize>,
    /// `true` only for the outermost call (the condition itself). Root nodes
    /// of "captureable container" kinds (Binary, Unary) are not captured —
    /// they reduce to `__cond` anyway.
    is_root: bool,
    /// `true` while descending into `Call` / `MethodCall` / `StaticMethodCall` args.
    /// Bare-`Ident` args in this position are function-reference coercions; we
    /// must not extract them into `let __vK = name;` (the inferencer would
    /// see `unknown` for the binding and break dispatch).
    in_call_arg: bool,
}

impl CaptureFolder {
    fn new(parent_id: AstId) -> Self {
        Self {
            parent_id,
            captures: Vec::new(),
            seen: IndexMap::default(),
            is_root: true,
            in_call_arg: false,
        }
    }

    fn finish(self) -> Vec<Capture> {
        self.captures
    }

    /// Fold the root condition expression, returning the rewritten form.
    fn fold_condition_expr(&mut self, expr: Expr) -> Expr {
        self.is_root = true;
        self.fold_expr(expr)
    }

    /// Add a capture (dedup'd by source text) and return the matching
    /// `Ident(__vK)` replacement.
    fn capture(&mut self, source: String, value: Expr, span: crate::token::Span) -> Expr {
        let name = if let Some(&idx) = self.seen.get(&source) {
            self.captures[idx].name.clone()
        } else {
            let idx = self.captures.len();
            let name = format!("__v{idx}");
            self.captures.push(Capture {
                name: name.clone(),
                source: source.clone(),
                value,
            });
            self.seen.insert(source, idx);
            name
        };
        Expr::Ident(IdentExpr {
            id: self.parent_id,
            name,
            segments: Vec::new(),
            type_args: Vec::new(),
            span,
        })
    }
}

impl AstFolder for CaptureFolder {
    fn fold_expr(&mut self, expr: Expr) -> Expr {
        // Snapshot the original source BEFORE folding children, otherwise the
        // captured text would already contain `__vK` references.
        let original_source = unparse_expr_simple(&expr);
        let span = expr.span();
        let is_root = std::mem::replace(&mut self.is_root, false);
        let in_call_arg = std::mem::replace(&mut self.in_call_arg, false);

        match expr {
            Expr::Ident(ident) => {
                if in_call_arg {
                    // Bare-ident call arg: function-reference coercion.
                    return Expr::Ident(ident);
                }
                self.capture(ident.name.clone(), Expr::Ident(ident), span)
            }
            Expr::Binary(_) => {
                let rewritten = default_fold_expr(self, expr);
                if is_root {
                    return rewritten;
                }
                self.capture(original_source, rewritten, span)
            }
            Expr::Unary(u) => {
                // Skip negated numeric literals: extracting them into an
                // intermediate breaks bidirectional coercion (e.g. an
                // `i64 == -50` site needs `-50` typed as `i64`, not `i32`).
                if u.op == UnaryOp::Neg
                    && matches!(&u.expr, Expr::Literal(lit) if matches!(&lit.value, Literal::Number(_)))
                {
                    return Expr::Unary(u);
                }
                // `&fn_name` is the function-reference coercion; extracting
                // either it or its operand loses the coercion context.
                if u.op == UnaryOp::Ref && matches!(&u.expr, Expr::Ident(_)) {
                    return Expr::Unary(u);
                }
                // `&mut <expr>` requires a mutable lvalue; a fresh
                // `let __v = <expr>` is immutable, so the reconstructed
                // `&mut __v` would fail to typecheck.
                if u.op == UnaryOp::MutRef {
                    return Expr::Unary(u);
                }
                let rewritten = default_fold_expr(self, Expr::Unary(u));
                if is_root {
                    return rewritten;
                }
                self.capture(original_source, rewritten, span)
            }
            Expr::Call(mut c) => {
                // Callee stays untouched: it is almost always a bare function
                // identifier and recursing through it would either capture it
                // (as a useless intermediate) or convert a direct call into an
                // indirect one.
                c.args = c
                    .args
                    .into_iter()
                    .map(|arg| {
                        self.in_call_arg = true;
                        self.fold_expr(arg)
                    })
                    .collect();
                let rewritten = Expr::Call(c);
                self.capture(original_source, rewritten, span)
            }
            Expr::MethodCall(mut m) => {
                // Receiver recursion is deferred (see desugar's notes on
                // `Inspect` / `Fn<…>` dispatch gaps).
                m.args = m
                    .args
                    .into_iter()
                    .map(|arg| {
                        self.in_call_arg = true;
                        self.fold_expr(arg)
                    })
                    .collect();
                let rewritten = Expr::MethodCall(m);
                self.capture(original_source, rewritten, span)
            }
            Expr::StaticMethodCall(mut s) => {
                s.args = s
                    .args
                    .into_iter()
                    .map(|arg| {
                        self.in_call_arg = true;
                        self.fold_expr(arg)
                    })
                    .collect();
                let rewritten = Expr::StaticMethodCall(s);
                self.capture(original_source, rewritten, span)
            }
            Expr::FieldAccess(_) | Expr::Index(_) => {
                // Receiver / index recursion deferred to a later step (mirrors
                // current MethodCall behavior). Capture the access whole.
                self.capture(original_source, expr, span)
            }
            // Other expressions are left as leaves for the power-assert
            // walk: they are not captured, and their children are not
            // recursed into. This intentionally mirrors the historical
            // desugar so the failure-message shape stays stable.
            _ => expr,
        }
    }
}

/// Build the template-string expression passed to `panic(...)`.
fn build_panic_message(
    assert_stmt: &AssertStmt,
    captures: &[Capture],
    parent_id: AstId,
    span: crate::token::Span,
) -> Expr {
    let mut parts: Vec<TemplatePart> = Vec::new();

    let make_loc = |value: Literal| {
        Expr::Literal(LiteralExpr {
            id: parent_id,
            value,
            span,
        })
    };

    // "Assertion failed in <#function> at <#file>:<#line>[: <msg>]"
    parts.push(TemplatePart::String("Assertion failed in ".to_string()));
    parts.push(TemplatePart::Interpolation {
        expr: Box::new(make_loc(Literal::LocationFunction)),
        format: None,
    });
    parts.push(TemplatePart::String(" at ".to_string()));
    parts.push(TemplatePart::Interpolation {
        expr: Box::new(make_loc(Literal::LocationFile)),
        format: None,
    });
    parts.push(TemplatePart::String(":".to_string()));
    parts.push(TemplatePart::Interpolation {
        expr: Box::new(make_loc(Literal::LocationLine)),
        format: None,
    });
    if let Some(msg) = &assert_stmt.message {
        parts.push(TemplatePart::String(": ".to_string()));
        parts.push(TemplatePart::Interpolation {
            expr: Box::new(msg.clone()),
            format: None,
        });
    }

    // Prefer the pre-desugar source captured by the desugar phase so
    // `condition: …` reads in the user's own words; fall back to a printout
    // of the (possibly-desugared) condition when the assert was constructed
    // outside the normal pipeline (e.g. unit tests).
    let condition_source = assert_stmt
        .original_condition_source
        .clone()
        .unwrap_or_else(|| unparse_expr_simple(&assert_stmt.condition));
    parts.push(TemplatePart::String(format!(
        "\ncondition: {condition_source}\n"
    )));

    for cap in captures {
        parts.push(TemplatePart::String(format!("{}: ", cap.source)));
        parts.push(TemplatePart::Interpolation {
            expr: Box::new(Expr::Ident(IdentExpr {
                id: parent_id,
                name: cap.name.clone(),
                segments: Vec::new(),
                type_args: Vec::new(),
                span,
            })),
            format: Some(FormatSpec {
                spec: "?".to_string(),
            }),
        });
        parts.push(TemplatePart::String("\n".to_string()));
    }

    Expr::TemplateString(Box::new(TemplateStringExpr {
        id: parent_id,
        parts,
        span,
    }))
}
