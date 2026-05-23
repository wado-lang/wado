//! Desugar `assert` at TIR-lowering time into a power-assert-style
//! expansion:
//!
//! ```text
//! __assert_N: {
//!     let __v0 = <intermediate0>;
//!     let __v1 = <intermediate1>;
//!     ...
//!     let __cond = <rewritten condition>;
//!     if !__cond {
//!         panic(`Assertion failed in {#function} at {#file}:{#line}[: msg]
//! condition: <source>
//! <intermediate0_source>: {__v0:?}
//! ...`);
//!     }
//! }
//! ```
//!
//! Interesting sub-expressions of `cond` are captured into `__vK` locals
//! that the rewritten condition (and the failure-message lines) refer
//! to, so side-effecting sub-terms evaluate exactly once.
//!
//! The capture walk is hand-written rather than a general visitor: only
//! the handful of expression shapes that have meaningful intermediate
//! values participate, and each makes its own decision about whether
//! to recurse into children (`Binary` / `Unary` do) or capture whole
//! (`MethodCall` / `FieldAccess` / `Index` do, to keep their
//! receiver-typed dispatch context). Variants not listed are treated as
//! opaque leaves.

use crate::ast::{
    self, AssertStmt, AstId, Block, CallExpr, Condition, Expr, ExprStmt, FormatSpec, IdentExpr,
    IfStmt, Literal, LiteralExpr, TemplatePart, TemplateStringExpr, UnaryExpr, UnaryOp,
};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{TirBlock, TirStmt, TirStmtKind};
use crate::unparse::unparse_expr_simple;

use super::Resolver;
use super::types::FunctionContext;

impl<H: CompilerHost> Resolver<'_, H> {
    /// Desugar `assert cond[, msg];` into TIR. See the module doc for
    /// the expansion shape.
    pub(super) fn desugar_assert(
        &mut self,
        assert_stmt: &AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let span = assert_stmt.span;

        // One synthetic `AstId` shared by every node this expansion
        // creates. Allocated above the module's parser range, so
        // `record_reference_opt` / `record_local_symbol` entries keyed
        // on it are unreachable from `Annotated::ast_id_at` and never
        // pollute LSP cursor → AstId lookups. Sharing one id across
        // multiple nodes is safe for the same reason — the collisions
        // are unreachable too.
        let synth_id = self.alloc_synth_ast_id(ctx);

        // Walk the condition: select sub-expressions to capture and
        // rewrite each in place to `Ident(__vK)`. The captured `value`
        // keeps the user's original `AstId`s so the use→def edges for
        // the user's idents still get recorded.
        let mut capture = CaptureFolder::new(synth_id);
        let rewritten_condition = capture.fold_condition_expr(assert_stmt.condition.clone());
        let captures = capture.finish();

        // Scope the synthetic locals to this expansion.
        ctx.enter_scope();
        let mut inner_stmts: Vec<TirStmt> = Vec::with_capacity(captures.len() + 2);

        // Emit each captured intermediate as a `TirStmt::Let` directly,
        // bypassing `resolve_let` so:
        //  - `add_local` receives `defining_ast_id = None` and skips
        //    the `local_symbols` insert that would otherwise key on
        //    `synth_id` and shadow other entries via LSP queries; and
        //  - we never synthesise a `Pattern::Ident` whose id would
        //    trigger `record_local_symbol` / `record_reference_opt`.
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

        // `let __cond = <rewritten condition>;`. We pass `None` for
        // the expected type: a `bool` expectation here would propagate
        // into branch types of an `Expr::If` / `Expr::Match` inside
        // the condition and reject valid asserts whose branches
        // produce a non-bool value compared against something else.
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
        // routed through `resolve_stmt`. No `let` lives inside, so the
        // symbol-pollution concern above does not apply; the standard
        // ident lookup of `__cond` and the template-string + `panic`
        // resolution paths take over.
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

        // `__assert_N` — one labeled block per assert, numbered in
        // source order within the enclosing function. The label
        // scopes any future `break __assert_N` and keeps the synthetic
        // locals named predictably in WIR dumps.
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

/// Walks a condition expression, replacing each captured sub-expression
/// in place with `Ident(__vK)` and recording the original (pre-rewrite)
/// source text and value in [`Capture`] entries.
struct CaptureFolder {
    synth_id: AstId,
    captures: Vec<Capture>,
    /// `source_text -> capture index` for dedup. Two structurally identical
    /// sub-expressions share one `__vK` so the failure message stays terse
    /// and we do not re-evaluate identical sub-terms.
    seen: IndexMap<String, usize>,
    /// `true` only for the outermost call (the condition itself). The
    /// root `Binary` / `Unary` is not captured because it would just
    /// duplicate `__cond`.
    is_root: bool,
    /// `true` while descending into `Call` / `MethodCall` /
    /// `StaticMethodCall` arguments. A bare `Ident` in that position
    /// is a function-reference coercion site; extracting it into
    /// `let __vK = name;` loses the coercion context and the
    /// inferencer would see `unknown` for the binding.
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

impl CaptureFolder {
    fn fold_expr(&mut self, expr: Expr) -> Expr {
        // Snapshot the original source before folding children — the
        // captured text must read in the user's words, not `__vK`.
        let original_source = unparse_expr_simple(&expr);
        let span = expr.span();
        let is_root = std::mem::replace(&mut self.is_root, false);
        let in_call_arg = std::mem::replace(&mut self.in_call_arg, false);

        match expr {
            Expr::Ident(ident) => {
                if in_call_arg {
                    // Function-reference coercion site — leave as-is.
                    return Expr::Ident(ident);
                }
                self.capture(ident.name.clone(), Expr::Ident(ident), span)
            }
            Expr::Binary(mut b) => {
                b.left = self.fold_expr(b.left);
                b.right = self.fold_expr(b.right);
                let rewritten = Expr::Binary(b);
                if is_root {
                    return rewritten;
                }
                self.capture(original_source, rewritten, span)
            }
            Expr::Unary(u) => {
                // Skip negated numeric literals: capturing them breaks
                // bidirectional coercion (e.g. `i64 == -50` needs `-50`
                // typed as `i64`, not `i32`).
                if u.op == UnaryOp::Neg
                    && matches!(&u.expr, Expr::Literal(lit) if matches!(&lit.value, Literal::Number(_)))
                {
                    return Expr::Unary(u);
                }
                // `&fn_name` is the function-reference coercion;
                // capturing either it or its operand loses the context.
                if u.op == UnaryOp::Ref && matches!(&u.expr, Expr::Ident(_)) {
                    return Expr::Unary(u);
                }
                // `&mut <expr>` requires a mutable lvalue; an
                // immutable `let __v = <expr>` would make the
                // reconstructed `&mut __v` reject at typecheck.
                if u.op == UnaryOp::MutRef {
                    return Expr::Unary(u);
                }
                let mut u = u;
                u.expr = self.fold_expr(u.expr);
                let rewritten = Expr::Unary(u);
                if is_root {
                    return rewritten;
                }
                self.capture(original_source, rewritten, span)
            }
            Expr::Call(mut c) => {
                // Callee stays untouched: it is almost always a bare
                // function ident, and capturing it would either
                // produce a useless intermediate or turn a direct
                // call into an indirect one.
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
                // Receiver recursion is intentionally skipped:
                // extracting `<recv>` into a temp forces auto-derived
                // `Inspect` on the receiver's type, which trips
                // unrelated gaps (`Fn<…>` and CM resource handles
                // have no `Inspect`; receiver-module-dispatch keyed
                // by bare mangled name confuses same-name generics
                // across modules).
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
                // Receiver / index recursion deferred (same reason as
                // `MethodCall`): capture the access whole.
                self.capture(original_source, expr, span)
            }
            // Every other `Expr` variant is treated as an opaque leaf:
            // it is neither captured nor recursed into. This keeps
            // the failure-message shape predictable on shapes (`If`,
            // `Match`, `Closure`, …) whose children are not
            // meaningfully inspectable in isolation.
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

    // The condition reaches `desugar_assert` unmodified, so its
    // printout reads in the user's words (`s matches { p }`,
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
