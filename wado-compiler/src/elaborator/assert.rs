//! Annotation pass for `assert`.
//!
//! Power-assert capture-slot discovery still runs here: the read-only AST
//! scanner picks which sub-expressions deserve a `let __vK = …;` binding, the
//! capture hook on [`Elaborator::resolve_expr`] flags each as `emitted` once
//! it actually fires during body resolution, and the recorded
//! [`super::sem::types::AssertCaptureInfo`] carries the result to reify.
//!
//! Reify rebuilds the expansion shape (`__assert_N` labeled block, `let
//! __cond = …`, `if !__cond { panic(<template>); }`, the per-slot
//! interpolation lines) from the AST + the recorded slot table.

use crate::ast::{AssertStmt, AstId, Expr, Literal, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{TirExpr, TirExprKind, TirStmt, TypeId};
use crate::unparse::unparse_expr_simple;

use super::Elaborator;
use super::types::FunctionContext;

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn desugar_assert(
        &mut self,
        assert_stmt: &AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        self.record_desugar(assert_stmt.id, super::sem::types::DesugarKind::Assert);

        // Phase 1: read-only AST scan to decide captures.
        let mut scanner = CaptureScanner::new();
        scanner.scan_root(&assert_stmt.condition);
        let CaptureScanner {
            slots,
            ast_id_to_slot,
            ..
        } = scanner;

        ctx.enter_scope();

        debug_assert!(ctx.assert_capture_ctx.is_none());
        ctx.assert_capture_ctx = Some(AssertCaptureContext {
            slots,
            ast_id_to_slot,
            in_progress: IndexSet::default(),
        });

        // Walk the condition for fact recording. The hook on `resolve_expr`
        // marks each `Capture::emitted` flag once it sees its slot's `AstId`.
        let cond_tir = self.resolve_expr(&assert_stmt.condition, ctx, None);

        // Reserve the outer-scope `__cond` slot so subsequent local-index
        // accounting in the enclosing function (closure capture
        // `outer_index`, recorded `MutCapture::outer_index`, etc.) stays in
        // lockstep with reify's expansion — reify's `reify_assert` also
        // allocates `__cond` at this point in its own walk.
        let _cond_local_index = ctx.add_local("__cond".to_string(), cond_tir.type_id, false, None);

        // Walk the assert message for fact recording too.
        if let Some(msg) = &assert_stmt.message {
            let _ = self.resolve_expr(msg, ctx, None);
        }

        let AssertCaptureContext {
            slots,
            ast_id_to_slot,
            ..
        } = ctx
            .assert_capture_ctx
            .take()
            .expect("assert_capture_ctx must survive resolution");

        // Stage 5 (Gap 5 of WEP 2026-05-26): record the capture-slot table
        // so reify can pick the same sub-expressions for `let __vK = …;`
        // materialisation. Invert `ast_id_to_slot` so the recorded `slots`
        // vector is indexed by slot (matching `__vK` naming).
        let mut slot_ast_ids: Vec<Option<AstId>> = vec![None; slots.len()];
        for (&ast_id, &slot_idx) in &ast_id_to_slot {
            if let Some(entry) = slot_ast_ids.get_mut(slot_idx) {
                *entry = Some(ast_id);
            }
        }
        let stage5_slots: Vec<super::sem::types::AssertSlot> = slots
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                slot_ast_ids[i].map(|ast_id| super::sem::types::AssertSlot {
                    ast_id,
                    capture_label: c.source.clone(),
                })
            })
            .collect();
        let emitted_slot_indices: Vec<u32> = slots
            .iter()
            .enumerate()
            .filter_map(|(i, c)| if c.emitted { Some(i as u32) } else { None })
            .collect();
        self.record_assert_captures(
            assert_stmt.id,
            super::sem::types::AssertCaptureInfo {
                slots: stage5_slots,
                emitted_slot_indices,
            },
        );

        ctx.exit_scope();

        // Bump the per-function assert serial so reify's `__assert_N`
        // label numbering stays in sync with the source order.
        ctx.next_assert_id += 1;

        // Placeholder — reify is the sole TIR source for the assert expansion.
        Vec::new()
    }

    /// Hook the body walk calls when it encounters an `AstId` flagged for
    /// power-assert capture. Marks the slot's `emitted` flag (so the recorded
    /// `AssertCaptureInfo` lists it) and recursively walks the sub-tree for
    /// its own fact recording. Returns a placeholder `TirExpr` whose type
    /// matches the resolved sub-tree's so outer typecheck contexts see the
    /// right type.
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
        let _local_index = ctx.add_local(cap_name, type_id, false, None);

        let cap_ctx = ctx
            .assert_capture_ctx
            .as_mut()
            .expect("assert_capture_ctx survives recursive resolve");
        cap_ctx.slots[slot_idx].emitted = true;

        TirExpr::new(TirExprKind::Unit, type_id, cap_span)
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
    /// (e.g. reflexive `T::from(T_val)` returns its argument directly, so
    /// the outer `Call` is never resolved as a node and the capture hook is
    /// never called on its `AstId`).
    emitted: bool,
}

/// Per-assert state carried on [`FunctionContext::assert_capture_ctx`]
/// while [`Elaborator::resolve_expr`] is resolving the condition.
pub(super) struct AssertCaptureContext {
    /// Pre-scanned captures, indexed by slot.
    slots: Vec<Capture>,
    /// AST node identity → slot index. The elaborator hook consults this
    /// at every `resolve_expr` entry.
    ast_id_to_slot: IndexMap<AstId, usize>,
    /// `AstId`s currently being recursively resolved by the hook. Stops
    /// the hook from re-firing on the same node during the inner
    /// `resolve_expr` call.
    in_progress: IndexSet<AstId>,
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
