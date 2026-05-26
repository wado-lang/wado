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
//!    resolution is in flight, [`Elaborator::resolve_expr`] consults the
//!    [`AssertCaptureContext`] side-channel on the function context.
//!    When the current `Expr`'s `AstId` is in the capture set, the
//!    elaborator allocates a fresh `__vK` local, emits a
//!    `TirStmt::Let { value: <recursively resolved expr>, ... }`,
//!    and returns `Local(__vK)` in place of the resolved sub-expression.
//!    The `in_progress` guard prevents the hook from re-firing on the
//!    same node during the recursive resolution.
//!
//! Because the hook fires on AST identity, not on TIR shape, the
//! elaborator-synthesised wrappers (auto-ref via
//! `adjust_receiver_for_self_kind`, literal coercions, reflexive
//! `T::from(T_val)` collapse, …) need no special handling: the wrappers
//! live *inside* the resolved TIR that becomes the captured `let __vK =
//! …;` value, and nodes that evaporate during resolution simply never
//! trigger the hook.

use crate::ast::{AssertStmt, AstId, Expr, Literal, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{
    CallArg, FunctionRef, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirTemplatePart,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::unparse::unparse_expr_simple;

use super::Elaborator;
use super::types::FunctionContext;

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Desugar `assert cond[, msg];` into TIR. See the module doc for
    /// the expansion shape.
    pub(super) fn desugar_assert(
        &mut self,
        assert_stmt: &AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let span = assert_stmt.span;

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
        // capture hook armed. The hook (in `Elaborator::resolve_expr`)
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

        // `if !__cond { panic(<template>); }` — built TIR-direct.
        //
        // The condition is `!Local(__cond)`. The panic call resolves
        // through a hand-built `FunctionRef` to `core:internal::panic`
        // (re-exported through `core:prelude`). The template message
        // is constructed as `TirExprKind::TemplateString`; only slots
        // whose capture hook fired contribute interpolation lines —
        // a slot stays `emitted == false` when its AST node evaporates
        // during resolution (e.g. reflexive `T::from(T_val)` returns
        // its argument directly, so the outer `Call` is never
        // resolved as a node and the corresponding `__vK` is never
        // bound). Referencing such an unbound local would later panic
        // in lowering.
        let cond_ref = TirExpr::new(
            TirExprKind::Local {
                index: cond_local_index,
                name: cond_name,
            },
            cond_type,
            span,
        );
        let neg_cond = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(cond_ref),
            },
            TypeTable::BOOL,
            span,
        );
        let template_tir = self.build_assert_panic_template(assert_stmt, &slots, ctx, span);
        let panic_call = self.build_panic_call(template_tir, span);
        let then_block = TirBlock::new(
            vec![TirStmt::new(TirStmtKind::Expr(panic_call), span)],
            span,
        );
        inner_stmts.push(TirStmt::new(
            TirStmtKind::If {
                condition: neg_cond,
                then_block,
                else_block: None,
            },
            span,
        ));

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

    /// Build the `TirExpr::TemplateString` passed to `panic(...)`. Slots
    /// whose capture hook never fired (their AST node evaporated during
    /// resolution) are skipped so the message never references an
    /// unbound `__vK`.
    fn build_assert_panic_template(
        &mut self,
        assert_stmt: &AssertStmt,
        slots: &[Capture],
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        let string_type = self.get_string_struct_type();
        let line = span.line as u64;
        // "Assertion failed in <#function> at <#file>:<#line>[: <msg>]\n"
        let mut parts: Vec<TirTemplatePart> = vec![
            TirTemplatePart::Literal("Assertion failed in ".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::StringLiteral(ctx.function_name.clone()),
                    string_type,
                    span,
                )),
                format_spec: None,
            },
            TirTemplatePart::Literal(" at ".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::StringLiteral(self.current_module_source.to_string()),
                    string_type,
                    span,
                )),
                format_spec: None,
            },
            TirTemplatePart::Literal(":".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    TypeTable::I32,
                    span,
                )),
                format_spec: None,
            },
        ];
        if let Some(msg) = &assert_stmt.message {
            parts.push(TirTemplatePart::Literal(": ".to_string()));
            let msg_tir = self.resolve_expr(msg, ctx, None);
            parts.push(TirTemplatePart::Interpolation {
                expr: Box::new(msg_tir),
                format_spec: None,
            });
        }

        // The condition reaches `desugar_assert` unmodified, so its
        // printout reads in the user's words (`s matches { p }`,
        // `a < b < c`, …) rather than the elaborator-internal expansion.
        let condition_source = unparse_expr_simple(&assert_stmt.condition);
        parts.push(TirTemplatePart::Literal(format!(
            "\ncondition: {condition_source}\n"
        )));

        for cap in slots {
            if !cap.emitted {
                continue;
            }
            // Look up the __vK local that the capture hook bound earlier.
            // The hook runs in this expansion's outer scope (entered at
            // `ctx.enter_scope` above), so the binding is reachable here.
            // If a future scanner extension ever fires the capture inside
            // a sub-expression's own scope and the binding has gone out
            // of scope by the time the template is built, fall back to
            // skipping the slot's interpolation rather than ICEing.
            let Some(lv) = ctx.lookup(&cap.name) else {
                continue;
            };
            parts.push(TirTemplatePart::Literal(format!("{}: ", cap.source)));
            let local_ref = TirExpr::new(
                TirExprKind::Local {
                    index: lv.index,
                    name: cap.name.clone(),
                },
                lv.type_id,
                span,
            );
            parts.push(TirTemplatePart::Interpolation {
                expr: Box::new(local_ref),
                format_spec: Some(crate::tir::TemplateFormatSpec {
                    fill: None,
                    align: None,
                    sign_plus: false,
                    alternate: false,
                    zero_pad: false,
                    width: None,
                    precision: None,
                    type_char: Some('?'),
                }),
            });
            parts.push(TirTemplatePart::Literal("\n".to_string()));
        }

        TirExpr::new(TirExprKind::TemplateString { parts }, string_type, span)
    }

    /// Build a TIR-direct call to `core:internal::panic(message)`. The
    /// `FunctionRef` is hand-constructed against the known canonical
    /// definition rather than re-routing through `resolve_call` (which
    /// would need an AST identifier and the prelude re-export plumbing).
    fn build_panic_call(&mut self, message: TirExpr, span: crate::token::Span) -> TirExpr {
        let module_source = self.interner.borrow_mut().core("internal");
        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source,
                    name: "panic".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: Vec::new(),
                args: vec![CallArg::new(message, false)],
            },
            TypeTable::NEVER,
            span,
        )
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

        // `defining_ast_id = None` so the synthetic `__vK` locals do not
        // enter `local_symbols` and pollute LSP hover / go-to-def lookups.
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
    /// `true` once the elaborator hook has fired for this slot. Slots that
    /// stay `false` had their AST node evaporate during resolution
    /// (e.g. reflexive `T::from(T_val)` returns its argument directly,
    /// so the outer `Call` is never resolved as a node and
    /// `resolve_expr` is never called on its `AstId`). Those slots are
    /// dropped from the failure message — `resolve_ident` would
    /// otherwise reject the unbound `__vK` reference.
    emitted: bool,
}

/// Per-assert state carried on [`FunctionContext::assert_capture_ctx`]
/// while [`Elaborator::resolve_expr`] is resolving the condition.
pub(super) struct AssertCaptureContext {
    /// Pre-scanned captures, indexed by slot. The elaborator hook reads
    /// `name` to produce the `let __vK` binding and writes `local` once
    /// the slot is realised.
    slots: Vec<Capture>,
    /// AST node identity → slot index. The elaborator hook consults this
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
/// originating [`AstId`] so the elaborator hook in `resolve_expr` can find
/// it.
///
/// One slot per capturable `Expr` — no source-text deduplication. Two
/// syntactically identical sub-terms (e.g. `f() == f()`) each get their
/// own `__vK` and evaluate independently, matching the source as
/// written. Such code is degenerate anyway; the dedup logic the old
/// AST-substitution `CaptureFolder` ran for it isn't worth the
/// complexity in the new hook.
struct CaptureScanner {
    slots: Vec<Capture>,
    /// `AstId` of each captureable sub-expression → its capture slot
    /// index.
    ast_id_to_slot: IndexMap<AstId, usize>,
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
            is_root: true,
            in_call_arg: false,
        }
    }

    fn scan_root(&mut self, expr: &Expr) {
        self.is_root = true;
        self.scan(expr);
    }

    /// Add a capture; the sub-expression's `AstId` is recorded so the
    /// elaborator hook can match it.
    fn add(&mut self, source: String, ast_id: AstId) {
        let idx = self.slots.len();
        let name = format!("__v{idx}");
        self.slots.push(Capture {
            name,
            source,
            emitted: false,
        });
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
