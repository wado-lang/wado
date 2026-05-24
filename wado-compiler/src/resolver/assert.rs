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
//! The pass runs in two phases, neither of which mutates the AST — the
//! source stays as the user wrote it, and LSP queries land on it
//! unchanged:
//!
//! 1. [`CaptureScanner`] walks the AST condition read-only, deciding
//!    which sub-expressions should be captured. Captures are deduped by
//!    source text (so `a + a` evaluates `a` once) and indexed by the
//!    sub-expression's [`AstId`].
//!
//! 2. The condition is resolved to TIR exactly once. While that
//!    resolution is in flight, [`Resolver::resolve_expr`] consults the
//!    [`AssertCaptureContext`] side-channel on the function context.
//!    When the current `Expr`'s `AstId` is in the capture set, the
//!    resolver allocates a fresh `__vK` local, emits a
//!    `TirStmt::Let { value: <recursively resolved expr>, ... }`,
//!    and returns `Local(__vK)` in place of the resolved sub-expression.
//!    The `in_progress` guard prevents the hook from re-firing on the
//!    same node during the recursive resolution.
//!
//! Because the hook fires on AST identity, not on TIR shape, the
//! resolver-synthesised wrappers (auto-ref via
//! `adjust_receiver_for_self_kind`, literal coercions, reflexive
//! `T::from(T_val)` collapse, …) need no special handling: the wrappers
//! live *inside* the resolved TIR that becomes the captured `let __vK =
//! …;` value, and nodes that evaporate during resolution simply never
//! trigger the hook.

use crate::ast::{
    self, AssertStmt, AstId, Block, CallExpr, Condition, Expr, ExprStmt, FormatSpec, IdentExpr,
    IfStmt, Literal, LiteralExpr, TemplatePart, TemplateStringExpr, UnaryExpr, UnaryOp,
};
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId};
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
        let CaptureScanner {
            slots,
            ast_id_to_slot,
            ..
        } = scanner;

        // Scope the synthetic locals to this expansion.
        ctx.enter_scope();

        // Phase 2: resolve the unmodified AST condition to TIR with the
        // capture hook armed. The hook (in `Resolver::resolve_expr`)
        // consumes `ast_id_to_slot` to decide which sub-expressions
        // become `let __vK = …;` bindings, and appends the bindings to
        // `emitted_lets` in inner-first order.
        //
        // We `replace` the field rather than `insert` it so a panic
        // inside resolution doesn't strand a stale context on
        // `FunctionContext` (and so nested asserts, were they ever
        // legal inside an assert condition, would assert!() here).
        debug_assert!(ctx.assert_capture_ctx.is_none());
        ctx.assert_capture_ctx = Some(AssertCaptureContext {
            slots,
            ast_id_to_slot,
            in_progress: IndexSet::default(),
            emitted_lets: Vec::new(),
        });

        // We pass `None` for the expected type: a `bool` expectation
        // here would propagate into branch types of an `Expr::If` /
        // `Expr::Match` inside the condition and reject valid asserts
        // whose branches produce a non-bool value compared against
        // something else.
        let cond_tir = self.resolve_expr(&assert_stmt.condition, ctx, None);

        let AssertCaptureContext {
            slots,
            emitted_lets,
            ..
        } = ctx
            .assert_capture_ctx
            .take()
            .expect("assert_capture_ctx must survive resolution");

        let mut inner_stmts: Vec<TirStmt> = Vec::with_capacity(emitted_lets.len() + 2);
        inner_stmts.extend(emitted_lets);

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
        //
        // Only slots whose hook fired contribute lines to the failure
        // message — a slot stays with `emitted == false` when its AST
        // node evaporates during resolution (e.g. reflexive
        // `T::from(T_val)` returns its argument directly, so the outer
        // `Call` is never resolved as a node and `resolve_expr` is
        // never called on its `AstId`). The corresponding `__vK` would
        // be unbound and `resolve_ident` would reject the template.
        let panic_message = build_panic_message(assert_stmt, &slots, synth_id, span);
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

    /// Handle a `resolve_expr` call whose `Expr` was flagged for
    /// power-assert capture by [`CaptureScanner`]. Re-enters
    /// `resolve_expr` to produce the resolved TIR for the sub-tree,
    /// emits `let __vK = <resolved>;` onto the capture context's
    /// pending-let buffer, and returns `Local(__vK)` so the surrounding
    /// resolution sees the binding in place of the original
    /// sub-expression.
    ///
    /// `in_progress` is set around the recursive call so the hook in
    /// `resolve_expr` doesn't fire again on the same `AstId` and recurse
    /// forever.
    pub(super) fn resolve_with_assert_capture(
        &mut self,
        ast_id: AstId,
        slot_idx: usize,
        expr: &Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        ctx.assert_capture_ctx
            .as_mut()
            .expect("assert_capture_ctx present (guarded by caller)")
            .in_progress
            .insert(ast_id);

        let resolved = self.resolve_expr(expr, ctx, expected_type);

        ctx.assert_capture_ctx
            .as_mut()
            .expect("assert_capture_ctx survives recursive resolve")
            .in_progress
            .shift_remove(&ast_id);

        let type_id = resolved.type_id;
        let cap_span = resolved.span;
        let cap_name = ctx
            .assert_capture_ctx
            .as_ref()
            .expect("assert_capture_ctx survives recursive resolve")
            .slots[slot_idx]
            .name
            .clone();

        // `add_local` receives `defining_ast_id = None` so the synthetic
        // locals do not leak into `local_symbols` (which would key on
        // `synth_id` and shadow other entries via LSP queries).
        let local_index = ctx.add_local(cap_name.clone(), type_id, false, None);

        let cap_ctx = ctx
            .assert_capture_ctx
            .as_mut()
            .expect("assert_capture_ctx survives recursive resolve");
        cap_ctx.slots[slot_idx].emitted = true;
        cap_ctx.emitted_lets.push(TirStmt::new(
            TirStmtKind::Let {
                name: cap_name.clone(),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: resolved,
                skip_value_copy: false,
            },
            cap_span,
        ));

        TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name: cap_name,
            },
            type_id,
            cap_span,
        )
    }
}

/// One sub-expression captured during the power-assert scan.
struct Capture {
    /// Variable name (`__v0`, `__v1`, …) the rewritten condition refers to.
    name: String,
    /// Source text of the original sub-expression, used in the failure message.
    source: String,
    /// Set to `true` once the resolver hook fires for this slot.
    /// Slots that stay `false` had their AST node evaporate during
    /// resolution (e.g. reflexive `T::from(T_val)` returns its argument
    /// directly, so the outer `Call` is never resolved as a node and
    /// `resolve_expr` is never called on its `AstId`). Those slots are
    /// dropped from the failure message — `resolve_ident` would
    /// otherwise reject the unbound `__vK` reference.
    emitted: bool,
}

/// Per-assert state carried on [`FunctionContext::assert_capture_ctx`]
/// while [`Resolver::resolve_expr`] is resolving the condition.
pub(super) struct AssertCaptureContext {
    /// Pre-scanned captures, indexed by slot. The resolver hook reads
    /// `name` to produce the `let __vK` binding and writes `local` once
    /// the slot is realised.
    slots: Vec<Capture>,
    /// AST node identity → slot index. The resolver hook consults this
    /// at every `resolve_expr` entry.
    ast_id_to_slot: IndexMap<AstId, usize>,
    /// `AstId`s currently being recursively resolved by the hook. Stops
    /// the hook from re-firing on the same node during the inner
    /// `resolve_expr` call.
    in_progress: IndexSet<AstId>,
    /// `let __vK = …;` bindings produced so far, in emission (inner →
    /// outer) order. Drained into `inner_stmts` once resolution
    /// finishes.
    emitted_lets: Vec<TirStmt>,
}

impl AssertCaptureContext {
    /// Slot index for `ast_id`, when (a) the scanner flagged it for
    /// capture and (b) it isn't already being recursively resolved by
    /// the hook itself.
    pub(super) fn slot_for(&self, ast_id: AstId) -> Option<usize> {
        if self.in_progress.contains(&ast_id) {
            return None;
        }
        self.ast_id_to_slot.get(&ast_id).copied()
    }
}

/// Read-only AST scanner: decides which sub-expressions of the assert
/// condition deserve a `__vK` capture and records each capture's
/// originating [`AstId`] so the resolver hook in `resolve_expr` can find
/// it.
struct CaptureScanner {
    slots: Vec<Capture>,
    /// `AstId` of each captureable sub-expression → its capture slot
    /// index. Two AST nodes with the same source text share one slot
    /// (dedup keeps the failure message terse and avoids re-evaluating
    /// identical sub-terms); both ids map to the same slot here.
    ast_id_to_slot: IndexMap<AstId, usize>,
    /// Source text → slot index, used to dedup before allocating a new
    /// `__vK`. Discarded after the scan.
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
            slots: Vec::new(),
            ast_id_to_slot: IndexMap::default(),
            source_to_idx: IndexMap::default(),
            is_root: true,
            in_call_arg: false,
        }
    }

    fn scan_root(&mut self, expr: &Expr) {
        self.is_root = true;
        self.scan(expr);
    }

    /// Add a capture (dedup'd by source text); the sub-expression's
    /// `AstId` is recorded so the resolver hook can match it.
    fn add(&mut self, source: String, ast_id: AstId) {
        let idx = if let Some(&idx) = self.source_to_idx.get(&source) {
            idx
        } else {
            let idx = self.slots.len();
            let name = format!("__v{idx}");
            self.slots.push(Capture {
                name,
                source: source.clone(),
                emitted: false,
            });
            self.source_to_idx.insert(source, idx);
            idx
        };
        self.ast_id_to_slot.insert(ast_id, idx);
    }

    fn scan(&mut self, expr: &Expr) {
        let ast_id = expr.id();
        let is_root = std::mem::replace(&mut self.is_root, false);
        let in_call_arg = std::mem::replace(&mut self.in_call_arg, false);

        match expr {
            Expr::Ident(ident) => {
                if in_call_arg {
                    // Function-reference coercion site — leave as-is.
                    return;
                }
                self.add(ident.name.clone(), ast_id);
            }
            Expr::Binary(b) => {
                self.scan(&b.left);
                self.scan(&b.right);
                if !is_root {
                    self.add(unparse_expr_simple(expr), ast_id);
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
                    self.add(unparse_expr_simple(expr), ast_id);
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
                self.add(unparse_expr_simple(expr), ast_id);
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
                self.add(unparse_expr_simple(expr), ast_id);
            }
            Expr::StaticMethodCall(s) => {
                for arg in &s.args {
                    self.in_call_arg = true;
                    self.scan(arg);
                }
                self.add(unparse_expr_simple(expr), ast_id);
            }
            Expr::FieldAccess(_) | Expr::Index(_) => {
                // Receiver / index recursion deferred (same reason as
                // `MethodCall`): capture the access whole.
                self.add(unparse_expr_simple(expr), ast_id);
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

/// Build the template-string expression passed to `panic(...)`. Slots
/// whose hook never fired (their AST node evaporated during resolution)
/// are skipped so the message never references an unbound `__vK`.
fn build_panic_message(
    assert_stmt: &AssertStmt,
    slots: &[Capture],
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

    for cap in slots {
        if !cap.emitted {
            continue;
        }
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
