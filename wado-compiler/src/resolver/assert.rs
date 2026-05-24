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
//! The pass runs in two phases so the AST is never mutated — it stays
//! the source of truth, and LSP queries land on the user's text as
//! written:
//!
//! 1. [`CaptureScanner`] walks the AST condition read-only, deciding
//!    which sub-expressions should be captured. Captures are deduped by
//!    source text (so `a + a` evaluates `a` once) and indexed by the
//!    sub-expression's source span.
//! 2. The condition is resolved to TIR untouched.
//!    [`TirCaptureWalker`] then traverses that TIR post-order; every
//!    `TirExpr` whose span matches a captured AST span is extracted
//!    into a fresh `let __vK = <tir>;` and replaced with `Local(__vK)`.
//!    Post-order keeps inner captures evaluated first.

use crate::ast::{
    self, AssertStmt, AstId, Block, CallExpr, Condition, Expr, ExprStmt, FormatSpec, IdentExpr,
    IfStmt, Literal, LiteralExpr, TemplatePart, TemplateStringExpr, UnaryExpr, UnaryOp,
};
use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId};
use crate::tir_visitor::TirMutVisitor;
use crate::token::Span;
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

        // Phase 1: read-only AST scan to decide captures.
        let mut scanner = CaptureScanner::new();
        scanner.scan_root(&assert_stmt.condition);
        let captures = scanner.captures;
        let span_to_capture = scanner.span_to_capture;

        // Scope the synthetic locals to this expansion.
        ctx.enter_scope();
        let mut inner_stmts: Vec<TirStmt> = Vec::with_capacity(captures.len() + 2);

        // Phase 2a: resolve the unmodified AST condition to TIR.
        // We pass `None` for the expected type: a `bool` expectation
        // here would propagate into branch types of an `Expr::If` /
        // `Expr::Match` inside the condition and reject valid asserts
        // whose branches produce a non-bool value compared against
        // something else.
        let cond_tir = self.resolve_expr(&assert_stmt.condition, ctx, None);

        // Phase 2b: walk the resulting TIR pre-order; each sub-expression
        // whose span matches a captured AST sub-expression is extracted
        // into a fresh `let __vK = ...;` (pushed onto `inner_stmts`) and
        // replaced with `Local(__vK)`.
        //
        // `add_local` receives `defining_ast_id = None` so the synthetic
        // locals do not leak into `local_symbols` (which would key on
        // `synth_id` and shadow other entries via LSP queries).
        //
        // `emitted_indexes` records which scanned captures actually
        // landed in the TIR — some AST sub-expressions evaporate during
        // resolution (e.g. reflexive `T::from(T_val)` returns its arg
        // unchanged) and have no matching `TirExpr.span` to extract. We
        // suppress those from the failure message below so it never
        // refers to an undeclared `__vK`.
        let (cond_tir, emitted_indexes) = {
            let mut walker = TirCaptureWalker {
                captures: &captures,
                span_to_capture: &span_to_capture,
                ctx,
                emitted: IndexMap::default(),
                emitted_lets: Vec::with_capacity(captures.len()),
                excluded_spans: Vec::new(),
            };
            let mut cond_tir = cond_tir;
            walker.visit_expr(&mut cond_tir);
            let emitted: Vec<usize> = walker.emitted.keys().copied().collect();
            inner_stmts.extend(walker.emitted_lets);
            (cond_tir, emitted)
        };
        let captures_for_message: Vec<&Capture> = emitted_indexes
            .iter()
            .map(|&idx| &captures[idx])
            .collect();

        // `let __cond = <cond_tir>;`
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
        let panic_message =
            build_panic_message(assert_stmt, &captures_for_message, synth_id, span);
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

/// One sub-expression captured during the power-assert scan.
struct Capture {
    /// Variable name (`__v0`, `__v1`, …) the rewritten condition refers to.
    name: String,
    /// Source text of the original sub-expression, used in the failure message.
    source: String,
}

/// Read-only AST scanner: decides which sub-expressions of the assert
/// condition deserve a `__vK` capture and records each capture's source
/// span so the matching TIR node can later be extracted by
/// [`TirCaptureWalker`].
struct CaptureScanner {
    captures: Vec<Capture>,
    /// Source span of each captureable sub-expression → its capture index.
    /// Two AST nodes with the same source text share one capture entry
    /// (dedup keeps the failure message terse and avoids re-evaluating
    /// identical sub-terms); both spans map to the same index here.
    span_to_capture: IndexMap<Span, usize>,
    /// Source text → capture index, used to dedup before allocating a
    /// new `__vK`. Discarded after the scan.
    source_to_idx: IndexMap<String, usize>,
    /// `true` only for the root call (the condition itself). The root
    /// `Binary` / `Unary` is not captured because it would just
    /// duplicate `__cond`.
    is_root: bool,
    /// `true` while descending into `Call` / `MethodCall` /
    /// `StaticMethodCall` arguments. A bare `Ident` in that position
    /// is a function-reference coercion site; extracting it into
    /// `let __vK = name;` would lose the coercion context and the
    /// inferencer would see `unknown` for the binding.
    in_call_arg: bool,
}

impl CaptureScanner {
    fn new() -> Self {
        Self {
            captures: Vec::new(),
            span_to_capture: IndexMap::default(),
            source_to_idx: IndexMap::default(),
            is_root: true,
            in_call_arg: false,
        }
    }

    fn scan_root(&mut self, expr: &Expr) {
        self.is_root = true;
        self.scan(expr);
    }

    /// Add a capture (dedup'd by source text); the sub-expression's span
    /// is recorded so the TIR walker can match the corresponding TIR
    /// node.
    fn add(&mut self, source: String, span: Span) {
        let idx = if let Some(&idx) = self.source_to_idx.get(&source) {
            idx
        } else {
            let idx = self.captures.len();
            let name = format!("__v{idx}");
            self.captures.push(Capture {
                name,
                source: source.clone(),
            });
            self.source_to_idx.insert(source, idx);
            idx
        };
        self.span_to_capture.insert(span, idx);
    }

    fn scan(&mut self, expr: &Expr) {
        let span = expr.span();
        let is_root = std::mem::replace(&mut self.is_root, false);
        let in_call_arg = std::mem::replace(&mut self.in_call_arg, false);

        match expr {
            Expr::Ident(ident) => {
                if in_call_arg {
                    // Function-reference coercion site — leave as-is.
                    return;
                }
                self.add(ident.name.clone(), span);
            }
            Expr::Binary(b) => {
                self.scan(&b.left);
                self.scan(&b.right);
                if !is_root {
                    self.add(unparse_expr_simple(expr), span);
                }
            }
            Expr::Unary(u) => {
                // Skip negated numeric literals: capturing them breaks
                // bidirectional coercion (e.g. `i64 == -50` needs `-50`
                // typed as `i64`, not `i32`).
                if u.op == UnaryOp::Neg
                    && matches!(&u.expr, Expr::Literal(lit) if matches!(&lit.value, Literal::Number(_)))
                {
                    return;
                }
                // `&fn_name` is the function-reference coercion;
                // capturing either it or its operand loses the context.
                if u.op == UnaryOp::Ref && matches!(&u.expr, Expr::Ident(_)) {
                    return;
                }
                // `&mut <expr>` requires a mutable lvalue; an
                // immutable `let __v = <expr>` would make the
                // reconstructed `&mut __v` reject at typecheck.
                if u.op == UnaryOp::MutRef {
                    return;
                }
                self.scan(&u.expr);
                if !is_root {
                    self.add(unparse_expr_simple(expr), span);
                }
            }
            Expr::Call(c) => {
                // Callee stays untouched: it is almost always a bare
                // function ident, and capturing it would either
                // produce a useless intermediate or turn a direct
                // call into an indirect one.
                for arg in &c.args {
                    self.in_call_arg = true;
                    self.scan(arg);
                }
                self.add(unparse_expr_simple(expr), span);
            }
            Expr::MethodCall(m) => {
                // Receiver recursion is intentionally skipped:
                // extracting `<recv>` into a temp forces auto-derived
                // `Inspect` on the receiver's type, which trips
                // unrelated gaps (`Fn<…>` and CM resource handles
                // have no `Inspect`; receiver-module-dispatch keyed
                // by bare mangled name confuses same-name generics
                // across modules).
                for arg in &m.args {
                    self.in_call_arg = true;
                    self.scan(arg);
                }
                self.add(unparse_expr_simple(expr), span);
            }
            Expr::StaticMethodCall(s) => {
                for arg in &s.args {
                    self.in_call_arg = true;
                    self.scan(arg);
                }
                self.add(unparse_expr_simple(expr), span);
            }
            Expr::FieldAccess(_) | Expr::Index(_) => {
                // Receiver / index recursion deferred (same reason as
                // `MethodCall`): capture the access whole.
                self.add(unparse_expr_simple(expr), span);
            }
            // Every other `Expr` variant is treated as an opaque leaf:
            // it is neither captured nor recursed into. This keeps
            // the failure-message shape predictable on shapes (`If`,
            // `Match`, `Closure`, …) whose children are not
            // meaningfully inspectable in isolation.
            _ => {}
        }
    }
}

/// Bookkeeping for a capture that's already been extracted: the TIR
/// walker stores this on first match so a second occurrence of the same
/// source span reuses the same local without emitting another `let`.
#[derive(Clone)]
struct EmittedCapture {
    name: String,
    local_index: u32,
    type_id: TypeId,
}

/// TIR walker: pre-order traversal of the resolved condition TIR. The
/// outermost `TirExpr` whose span matches a [`CaptureScanner`] entry is
/// extracted into a fresh `let __vK = …;` (appended to `emitted_lets`)
/// and replaced with `Local(__vK)`. The walker then recurses into the
/// captured subtree to surface any nested captures (so inner `__vK`s
/// bind before the outer one that references them).
///
/// Span exclusion: the resolver inherits the parent's span when it
/// synthesises auto-ref / auto-deref wrappers around a receiver (see
/// `adjust_receiver_for_self_kind`). Naively matching by span would
/// trip on those wrappers and capture them instead of the outer
/// method call. `excluded_spans` holds the spans we've already
/// captured at, so when we descend into the captured subtree the
/// immediate synthesised children (which share the parent's span) are
/// passed through untouched.
struct TirCaptureWalker<'a, 'ctx> {
    captures: &'a [Capture],
    span_to_capture: &'a IndexMap<Span, usize>,
    ctx: &'ctx mut FunctionContext,
    emitted: IndexMap<usize, EmittedCapture>,
    emitted_lets: Vec<TirStmt>,
    excluded_spans: Vec<Span>,
}

impl TirMutVisitor for TirCaptureWalker<'_, '_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // Resolver-synthesised wrapper around an already-captured node:
        // recurse but never match.
        if self.excluded_spans.contains(&expr.span) {
            self.walk_expr(expr);
            return;
        }

        let Some(&cap_idx) = self.span_to_capture.get(&expr.span) else {
            // Not a capture target; walk children normally.
            self.walk_expr(expr);
            return;
        };

        if let Some(emitted) = self.emitted.get(&cap_idx).cloned() {
            // Same source text already captured — reuse its local
            // instead of emitting another `let`.
            *expr = TirExpr::new(
                TirExprKind::Local {
                    index: emitted.local_index,
                    name: emitted.name,
                },
                emitted.type_id,
                expr.span,
            );
            return;
        }

        // Take the matching TIR subtree out so we can recurse into it
        // for nested captures. The placeholder we leave behind is
        // overwritten with the final `Local(__vK)` below.
        let cap_name = self.captures[cap_idx].name.clone();
        let type_id = expr.type_id;
        let cap_span = expr.span;
        let local_index = self.ctx.add_local(cap_name.clone(), type_id, false, None);
        let mut captured = std::mem::replace(
            expr,
            TirExpr::new(
                TirExprKind::Local {
                    index: local_index,
                    name: cap_name.clone(),
                },
                type_id,
                cap_span,
            ),
        );

        // Surface nested captures inside `captured`. The captured node
        // itself is skipped (we're already capturing it); its
        // descendants are walked, with the same-span synthesised
        // wrappers excluded by `excluded_spans`.
        self.excluded_spans.push(cap_span);
        self.walk_expr(&mut captured);
        self.excluded_spans.pop();

        self.emitted_lets.push(TirStmt::new(
            TirStmtKind::Let {
                name: cap_name.clone(),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: captured,
                skip_value_copy: false,
            },
            cap_span,
        ));
        self.emitted.insert(
            cap_idx,
            EmittedCapture {
                name: cap_name,
                local_index,
                type_id,
            },
        );
    }
}

/// Build the template-string expression passed to `panic(...)`. The
/// `captures` slice must contain only entries the TIR walker actually
/// emitted (filtered by `desugar_assert`), so every `__vK` referenced
/// here is guaranteed to be in scope.
fn build_panic_message(
    assert_stmt: &AssertStmt,
    captures: &[&Capture],
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
