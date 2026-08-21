//! Annotate `assert`: pick the operands a failure quotes and mark each one
//! conditional where a short-circuit can skip it. See
//! `docs/wep-2026-08-19-power-assert-coverage.md`.

use crate::ast::{self, AssertStmt, AstId, BinaryOp, Expr, Literal, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::TypeId;
use crate::unparse::unparse_expr_source;

use super::Elaborator;
use super::types::FunctionContext;

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn desugar_assert(&mut self, assert_stmt: &AssertStmt, ctx: &mut FunctionContext) {
        self.record_desugar(assert_stmt.id, super::sem::types::DesugarKind::Assert);

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

        let cond_type = self.resolve_condition_expr(&assert_stmt.condition, ctx);

        // Reserved here because `reify_assert` allocates `__cond` at this
        // point too, and the two walks must stay in local-index lockstep.
        let _cond_local_index = ctx.add_local("__cond".to_string(), cond_type, false, None);

        // Matches the cold-branch allocation in `reify_assert`.
        let conditional_names: Vec<String> = ctx
            .assert_capture_ctx
            .as_ref()
            .expect("assert_capture_ctx survives resolution")
            .slots
            .iter()
            .filter(|c| c.conditional)
            .map(|c| render_local_name(&c.name))
            .collect();
        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        for name in conditional_names {
            ctx.add_local(name, string_type, false, None);
        }

        // Walk the assert message for fact recording too.
        if let Some(msg) = &assert_stmt.message {
            self.resolve_expr(msg, ctx, None);
        }

        let AssertCaptureContext {
            slots,
            ast_id_to_slot,
            ..
        } = ctx
            .assert_capture_ctx
            .take()
            .expect("assert_capture_ctx must survive resolution");

        // WEP 2026-05-26: record the capture-slot table
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
                    conditional: c.conditional,
                    is_place: c.is_place,
                })
            })
            .collect();
        self.record_assert_captures(
            assert_stmt.id,
            super::sem::types::AssertCaptureInfo {
                condition_source: unparse_expr_source(&assert_stmt.condition),
                line: assert_stmt.span.line,
                slots: stage5_slots,
            },
        );

        ctx.exit_scope();

        // Keeps reify's `__assert_N` labels in source order.
        ctx.next_assert_id += 1;
    }

    /// Hook the body walk calls on an `AstId` flagged for capture: resolves
    /// the sub-tree for fact recording and allocates the slot's locals.
    pub(super) fn resolve_with_assert_capture(
        &mut self,
        ast_id: AstId,
        slot_idx: usize,
        expr: &Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TypeId {
        ctx.assert_capture_ctx
            .as_mut()
            .expect("assert_capture_ctx present (guarded by caller)")
            .in_progress
            .insert(ast_id);

        let type_id = self.resolve_expr(expr, ctx, expected_type);

        ctx.assert_capture_ctx
            .as_mut()
            .expect("assert_capture_ctx survives recursive resolve")
            .in_progress
            .shift_remove(&ast_id);

        let (cap_name, conditional, is_place) = {
            let cap = &ctx
                .assert_capture_ctx
                .as_ref()
                .expect("assert_capture_ctx survives recursive resolve")
                .slots[slot_idx];
            (cap.name.clone(), cap.conditional, cap.is_place)
        };

        // Index accounting only: reify allocates the same locals in the same
        // order and is the side that emits their bindings. `defining_ast_id =
        // None` keeps them out of LSP hover / go-to-def.
        if is_place && !conditional {
            // Re-read; no local of its own.
        } else {
            ctx.add_local(cap_name.clone(), type_id, conditional, None);
            if conditional {
                ctx.add_local(
                    seen_local_name(&cap_name),
                    crate::tir::TypeTable::BOOL,
                    true,
                    None,
                );
            }
        }

        type_id
    }
}

/// Name of the flag recording whether a conditional slot's capture site ran.
pub(super) fn seen_local_name(cap_name: &str) -> String {
    format!("{cap_name}_seen")
}

/// Name of the cold-branch local holding a conditional slot's rendered text.
pub(super) fn render_local_name(cap_name: &str) -> String {
    format!("{cap_name}_text")
}

/// The text a conditional slot renders when the run never reached it.
pub(super) const NOT_EVALUATED: &str = "<not evaluated>";

/// Render every recorded capture plan, for `wado dump --assert-plan`.
pub(crate) fn render_plans(sem: &super::sem::ModuleSemantics) -> String {
    let mut out = String::new();
    for info in sem.types.assert_captures.values() {
        out.push_str(&format!(
            "{}: assert {}\n",
            info.line, info.condition_source
        ));
        if info.slots.is_empty() {
            out.push_str("  (no operand captured)\n");
            continue;
        }
        for (i, slot) in info.slots.iter().enumerate() {
            let reach = if slot.conditional {
                "conditional"
            } else {
                "always"
            };
            out.push_str(&format!("  __v{i}  {reach:<11}  {}\n", slot.capture_label));
        }
    }
    out
}

/// One sub-expression captured during the power-assert scan.
struct Capture {
    /// Variable name (`__v0`, `__v1`, …) the rewritten condition refers to.
    name: String,
    /// Source text of the original sub-expression, used in the failure message.
    source: String,
    /// See [`super::sem::types::AssertSlot::conditional`].
    conditional: bool,
    /// See [`super::sem::types::AssertSlot::is_place`].
    is_place: bool,
}

/// Whether the failure branch can re-read `expr` instead of binding it. Only a
/// binding qualifies: a field read moved into the cold branch shifts what field
/// scalarization sees.
fn is_place_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Ident(_))
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

/// Decides which sub-expressions of an assert condition become capture slots.
/// No source-text dedup: `f() == f()` gets a slot per occurrence, so each
/// evaluates as the source wrote it. Receivers and `matches` scrutinees are
/// never scanned — see the WEP's *Known gaps*.
struct CaptureScanner {
    slots: Vec<Capture>,
    ast_id_to_slot: IndexMap<AstId, usize>,
    /// The condition itself, which `__cond` already holds.
    is_root: bool,
    /// A bare `Ident` here may be a function-reference coercion site, and a
    /// binding would lose that context.
    in_call_arg: bool,
    /// A short-circuit lies above, so the capture may go unevaluated.
    conditional: bool,
}

impl CaptureScanner {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            ast_id_to_slot: IndexMap::default(),
            is_root: true,
            in_call_arg: false,
            conditional: false,
        }
    }

    fn scan_root(&mut self, expr: &Expr) {
        self.is_root = true;
        self.scan(expr);
    }

    fn add(&mut self, source: String, ast_id: AstId, is_place: bool) {
        let idx = self.slots.len();
        let name = format!("__v{idx}");
        let conditional = self.conditional;
        self.slots.push(Capture {
            name,
            source,
            conditional,
            is_place,
        });
        self.ast_id_to_slot.insert(ast_id, idx);
    }

    fn scan(&mut self, expr: &Expr) {
        let ast_id = expr.id();
        let is_root = std::mem::replace(&mut self.is_root, false);
        let in_call_arg = std::mem::replace(&mut self.in_call_arg, false);
        let conditional = self.conditional;

        match expr {
            Expr::Ident(ident) => {
                if in_call_arg {
                    // Function-reference coercion site — leave as-is.
                    return;
                }
                self.add(ident.name.clone(), ast_id, true);
            }
            Expr::Binary(b) => {
                self.scan(&b.left);
                self.conditional = conditional || matches!(b.op, BinaryOp::And | BinaryOp::Or);
                self.scan(&b.right);
                self.conditional = conditional;
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Unary(u) => {
                // A binding would type `-50` as `i32`, losing the bidirectional
                // coercion `i64 == -50` needs.
                if u.op == UnaryOp::Neg
                    && matches!(&u.expr, Expr::Literal(lit) if matches!(&lit.value, Literal::Number(_)))
                {
                    return;
                }
                // `&fn_name` is the function-reference coercion, lost either way.
                if u.op == UnaryOp::Ref && matches!(&u.expr, Expr::Ident(_)) {
                    return;
                }
                // `&mut` needs a mutable lvalue; the binding is immutable.
                if u.op == UnaryOp::MutRef {
                    return;
                }
                self.scan(&u.expr);
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Call(c) => {
                // The callee is left alone: capturing it would turn a direct
                // call into an indirect one.
                for arg in &c.args {
                    self.in_call_arg = true;
                    self.scan(arg);
                }
                self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
            }
            Expr::MethodCall(m) => {
                for arg in &m.args {
                    self.in_call_arg = true;
                    self.scan(arg);
                }
                self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
            }
            Expr::StaticMethodCall(s) => {
                for arg in &s.args {
                    self.in_call_arg = true;
                    self.scan(arg);
                }
                self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
            }
            Expr::ComparisonChain(chain) => {
                self.scan(&chain.first);
                for (idx, cmp) in chain.comparisons.iter().enumerate() {
                    // `a < b < c` runs as `(a < b) && (b < c)`.
                    self.conditional = conditional || idx >= 1;
                    self.scan(&cmp.right);
                }
                self.conditional = conditional;
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Cast(c) => {
                self.scan(&c.expr);
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Matches(_) => {
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Index(i) => {
                self.scan(&i.index);
                self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
            }
            Expr::TupleLiteral(t) => {
                // A literal takes its shape from the expected type, which a
                // binding would drop; its elements are ordinary operands.
                for elem in &t.elements {
                    self.scan(elem);
                }
            }
            Expr::StructLiteral(sl) => {
                for field in &sl.fields {
                    self.scan(&field.value);
                }
            }
            Expr::FieldAccess(_) => {
                self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
            }
            Expr::TemplateString(_) => {
                // The rendered string only: a captured interpolation would
                // evaluate twice, once for its slot and once for the template.
                self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
            }
            Expr::If(i) => {
                // Bodies are not walked: this node's own capture is the value
                // of the branch the run took. Its condition chose that branch.
                match &i.condition {
                    ast::Condition::Expr(cond) => self.scan(cond),
                    ast::Condition::LetChain { elements, .. } => {
                        for element in elements {
                            match element {
                                ast::ConditionElement::Let { expr, .. } => self.scan(expr),
                                ast::ConditionElement::Expr(cond) => self.scan(cond),
                            }
                        }
                    }
                }
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Match(m) => {
                // Arms are not walked, for the reason `If` bodies are not.
                self.scan(&m.expr);
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Block(_) | Expr::LabeledBlock(_) => {
                // Statements are not walked: a binding inside one is not an
                // operand of the condition.
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::Range(r) => {
                self.scan(&r.start);
                self.scan(&r.end);
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            Expr::TryOp(t) => {
                self.scan(&t.expr);
                if !is_root {
                    self.add(unparse_expr_source(expr), ast_id, is_place_expr(expr));
                }
            }
            // Neither captured nor walked; the WEP's *Deliberately out of
            // scope* has the reason for each.
            Expr::Literal(_)
            | Expr::Closure(_)
            | Expr::WithHandler(_)
            | Expr::Resume(_)
            | Expr::Spread(_, _)
            | Expr::Assign(_)
            | Expr::CompoundAssign(_)
            | Expr::TupleComprehension(_)
            | Expr::Error(_) => {}
        }
    }
}
