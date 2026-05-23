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
    IfStmt, Literal, LiteralExpr, TemplatePart, TemplateStringExpr, UnaryExpr, UnaryOp,
};
use crate::ast_folder::{AstFolder, default_fold_expr};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{TirBlock, TirStmt, TirStmtKind};
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

        // Every synthetic AST node produced by this expansion uses one
        // shared `AstId` allocated above `Module::ast_id_count()`. That
        // keeps the synthetic id disjoint from every parser-allocated id,
        // so any `record_reference_opt` / `record_local_symbol` writes
        // routed through it are unreachable from `Annotated::ast_id_at`
        // — LSP queries that translate a cursor position to an `AstId`
        // never see this id, and therefore never see the synthetic
        // entries.
        //
        // Sharing one id across the synthetic nodes (instead of one per
        // node) is fine because those collisions are themselves
        // unreachable from LSP for the same reason.
        let synth_id = self.alloc_synth_ast_id(ctx);

        // Power-assert capture: rewrite the condition so each interesting
        // sub-expression is replaced by a fresh `__vK` identifier, with a
        // matching `(source_text, expr)` pair recorded for later let binding.
        // The captured `value` keeps the user's original `AstId`s, so
        // resolving it still records use→def edges for the user's idents
        // exactly as if no rewrite had happened.
        let mut capture = CaptureFolder::new(synth_id);
        let rewritten_condition = capture.fold_condition_expr(assert_stmt.condition.clone());
        let captures = capture.finish();

        // Allocate a fresh scope for the synthetic locals so they fall out
        // of `ctx.scopes` when this expansion finishes.
        ctx.enter_scope();
        let mut inner_stmts: Vec<TirStmt> = Vec::with_capacity(captures.len() + 2);

        // Emit each captured intermediate as a TIR `Let` directly.
        //
        // We deliberately skip `Resolver::resolve_let` here, for two reasons:
        // 1. We pass `defining_ast_id: None` to `add_local`, so synthetic
        //    locals do not register `local_symbols` entries keyed on
        //    `assert_stmt.id` — that would shadow the assert in LSP queries
        //    such as `Annotated::symbol_at(assert_stmt.id)`.
        // 2. We never synthesise a `Pattern::Ident { id: synth_id, … }`,
        //    so no `record_local_symbol` / `record_reference_opt` writes
        //    are made against `synth_id` either.
        for cap in &captures {
            let value_tir = self.resolve_expr(&cap.value, ctx, None);
            let type_id = value_tir.type_id;
            let local_index = ctx.add_local(cap.name.clone(), type_id, false, None);
            inner_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: cap.name.clone(),
                    local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id,
                    value: value_tir,
                    skip_value_copy: false,
                },
                span,
            ));
        }

        // `let __cond = <rewritten condition>;` — resolved without a type
        // expectation so the condition's natural shape decides typing
        // (matches the historic `let __cond = …;` synthesised by desugar).
        // A `bool` expectation here would propagate into branch types of
        // `Expr::If` / `Expr::Match` etc. and break conditions that
        // contain those constructs.
        let cond_tir = self.resolve_expr(&rewritten_condition, ctx, None);
        let cond_type = cond_tir.type_id;
        let cond_name = "__cond".to_string();
        let cond_local_index = ctx.add_local(cond_name.clone(), cond_type, false, None);
        inner_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: cond_name.clone(),
                local_index: cond_local_index,
                is_mut: false,
                is_reactive: false,
                type_id: cond_type,
                value: cond_tir,
                skip_value_copy: false,
            },
            span,
        ));

        // `if !__cond { panic(<template>); }` — synthesised as AST and
        // routed through `resolve_stmt`. The block has no `let`, so the
        // symbol-pollution concern above does not apply here. We rely on
        // the standard `resolve_ident` lookup of `__cond` and the
        // template-string + `panic` resolution paths.
        let panic_message = build_panic_message(assert_stmt, &captures, synth_id, span);
        let panic_call = Expr::Call(Box::new(CallExpr {
            id: synth_id,
            callee: Expr::Ident(IdentExpr {
                id: synth_id,
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
        let if_ast = ast::Stmt::If(IfStmt {
            id: synth_id,
            condition: Condition::Expr(Expr::Unary(Box::new(UnaryExpr {
                id: synth_id,
                op: UnaryOp::Not,
                expr: Expr::Ident(IdentExpr {
                    id: synth_id,
                    name: cond_name,
                    segments: Vec::new(),
                    type_args: Vec::new(),
                    span,
                }),
                span,
            }))),
            then_block: Block {
                id: synth_id,
                stmts: vec![ast::Stmt::Expr(ExprStmt {
                    id: synth_id,
                    expr: panic_call,
                    span,
                })],
                span,
            },
            else_block: None,
            span,
        });
        inner_stmts.extend(self.resolve_stmt(&if_ast, ctx));

        ctx.exit_scope();

        // Wrap the whole expansion in a labeled block. The label uses a
        // per-function serial counter so the Nth assert in a function is
        // `__assert_{N-1}` — short, monotonic, and matches the historical
        // desugar output that downstream WIR dumps assume.
        let assert_serial = ctx.next_assert_id;
        ctx.next_assert_id += 1;
        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: format!("__assert_{assert_serial}"),
                block: TirBlock::new(inner_stmts, span),
            },
            span,
        )]
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
    synth_id: AstId,
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
    fn new(synth_id: AstId) -> Self {
        Self {
            synth_id,
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
            id: self.synth_id,
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
    synth_id: AstId,
    span: crate::token::Span,
) -> Expr {
    let mut parts: Vec<TemplatePart> = Vec::new();

    let make_loc = |value: Literal| {
        Expr::Literal(LiteralExpr {
            id: synth_id,
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

    // The condition reaches lower_assert unmodified by desugar, so its
    // printout reads in the user's own words (`s matches { p }`,
    // `a < b < c`, …) rather than the resolver-internal expansion.
    let condition_source = unparse_expr_simple(&assert_stmt.condition);
    parts.push(TemplatePart::String(format!(
        "\ncondition: {condition_source}\n"
    )));

    for cap in captures {
        parts.push(TemplatePart::String(format!("{}: ", cap.source)));
        parts.push(TemplatePart::Interpolation {
            expr: Box::new(Expr::Ident(IdentExpr {
                id: synth_id,
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
        id: synth_id,
        parts,
        span,
    }))
}
