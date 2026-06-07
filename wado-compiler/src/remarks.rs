//! Optimizer remarks: surface residual value-semantic copies that survive
//! optimization. See WEP `wep-2026-06-03-optimizer-remarks.md`.
//!
//! Wado deep-copies aggregates on assignment, parameter passing, and return.
//! A large part of the optimizer exists to remove those hidden copies, and most
//! of them are removed; the ones that remain are invisible while coding. After
//! the NIR optimization pipeline, a surviving copy appears as one of:
//!
//! - a call to a synthesized `$value_copy$T` deep-copy helper,
//! - a `builtin::array_clone` / `array_clone_shallow` call — the lowered (and
//!   possibly `value_copy_demote`-shallowed) spine copy of a `List<T>` or
//!   `String`, or
//! - a `builtin::copy_value` call — a direct deep copy of a value.
//!
//! NIR is the last IR that still carries per-expression source spans — WIR
//! instructions do not — and `wir_build` lowers these copies one-to-one, so
//! walking the optimized NIR yields the residual-copy set with exact source
//! locations.
//!
//! Detection is restricted to the entry module's functions. This excludes the
//! pervasive buffer copies inside stdlib helpers (`String::push` growth, …) and,
//! as a current limitation, also copies in other user modules of a multi-file
//! program (see the WEP's "broaden beyond the entry module" item). `array_copy`
//! is excluded regardless: it is bulk buffer movement, not a value-semantic copy.

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef};
use crate::nir_package::NirPackage;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

/// A single optimizer remark: a residual cost with its source span.
pub struct Remark {
    /// The remark text, without the `remark:` prefix the logger adds.
    pub message: String,
    /// Where the copy survives, in the original source.
    pub span: Span,
}

/// Collect remarks for value-semantic copies that survive optimization,
/// restricted to functions defined in the entry module.
pub fn collect_value_copy_remarks(package: &NirPackage) -> Vec<Remark> {
    let value_copy_set = package.value_copy_helper_types();

    let type_table_ref = package.type_table.borrow();
    let type_table: &TypeTable = &type_table_ref;
    let mut remarks = Vec::new();
    for func_rc in &package.functions {
        let func = func_rc.borrow();
        // A value-copy helper is synthesized in the module that defines the
        // copied type, so a helper for an entry-module type also lives in the
        // entry module. Skipping helper *bodies* keeps the copies inside a
        // helper from being reported against the helper itself.
        if func.module_source != package.entry_module_source || func.is_value_copy() {
            continue;
        }
        let Some(body) = &func.body else {
            continue;
        };
        let mut collector = Collector {
            value_copy_set: &value_copy_set,
            type_table,
            remarks: &mut remarks,
            current_span: body.blocks[body.root].span,
            in_value_block: false,
        };
        collector.scan_node(body, NodeRef::Block(body.root));
    }
    remarks
}

struct Collector<'a> {
    value_copy_set: &'a IndexMap<(ModuleSource, String), TypeId>,
    type_table: &'a TypeTable,
    remarks: &'a mut Vec<Remark>,
    /// Span of the enclosing real statement. Synthesized copy nodes
    /// (`array_clone`, demoted spine copies) carry placeholder spans, so the
    /// remark points at the statement that performs the copy (`let mut b = a;`).
    current_span: Span,
    /// True while walking a block used as an expression value, whose inner
    /// statements are synthesized and must not override `current_span`.
    in_value_block: bool,
}

impl Collector<'_> {
    /// If the expression `id` is a surviving value-copy operation, return the
    /// type whose value is copied.
    fn copied_type(&self, body: &Body, id: ExprId) -> Option<TypeId> {
        let node = &body.exprs[id];
        let ExprKind::Call { func, args, .. } = &node.kind else {
            return None;
        };
        // Deep `$value_copy$T` helper call.
        if args.len() == 1
            && let Some(&type_id) = self
                .value_copy_set
                .get(&(func.module_source.clone(), func.name.clone()))
        {
            return Some(type_id);
        }
        // Lowered / demoted copies. `array_copy` is excluded on purpose: it is
        // bulk buffer movement inside stdlib helpers, not a value-semantic copy.
        match func.monomorphized_builtin_name().as_deref() {
            // `array_clone(&agg.repr)` copies a `List<T>` / `String` backing
            // array; recover the owning aggregate type from the argument.
            Some("builtin::array_clone" | "builtin::array_clone_shallow") => {
                args.first().map(|a| clone_source_type(body, a.expr))
            }
            // `copy_value(v)` deep-copies a value directly, so the call's result
            // type is the copied type.
            Some("builtin::copy_value") => Some(node.type_id),
            _ => None,
        }
    }

    /// Walk the arena body parent-before-children, mirroring the previous tree
    /// visitor: a real statement sets the current span, a block-as-value freezes
    /// it (its synthesized inner statements carry placeholder spans), and a
    /// surviving copy expression is reported against the current span.
    fn scan_node(&mut self, body: &Body, node: NodeRef) {
        match node {
            NodeRef::Stmt(s) => {
                if self.in_value_block {
                    // Inside a block-as-value: keep the enclosing real
                    // statement's span rather than the synthesized inner
                    // statement's placeholder span.
                    self.scan_children(body, node);
                } else {
                    let prev = self.current_span;
                    self.current_span = body.stmts[s].span;
                    self.scan_children(body, node);
                    self.current_span = prev;
                }
            }
            NodeRef::Expr(e) => {
                if let Some(type_id) = self.copied_type(body, e) {
                    let type_name = self.type_table.type_name(type_id);
                    self.remarks.push(Remark {
                        message: format!("a copy of `{type_name}` survives optimization"),
                        span: self.current_span,
                    });
                }
                if is_value_block(body, e) {
                    let prev = self.in_value_block;
                    self.in_value_block = true;
                    self.scan_children(body, node);
                    self.in_value_block = prev;
                } else {
                    self.scan_children(body, node);
                }
            }
            NodeRef::Block(_) | NodeRef::Pat(_) => self.scan_children(body, node),
        }
    }

    fn scan_children(&mut self, body: &Body, node: NodeRef) {
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            self.scan_node(body, c);
        }
    }
}

/// Block-shaped expressions used as a value. Their inner statement lists come
/// from lowering / SROA reconstruction and carry placeholder spans, so the
/// remark anchors to the enclosing real statement instead of descending into
/// them.
fn is_value_block(body: &Body, id: ExprId) -> bool {
    matches!(
        body.exprs[id].kind,
        ExprKind::Block(_)
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. }
            | ExprKind::LabeledBlock { .. }
    )
}

/// For an `array_clone(&agg.repr)` call, recover the aggregate type (`List<T>`
/// or `String`) that owns the cloned `repr`, peeling the `&`/`&mut` reference
/// and the `.repr` field access. Falls back to the argument's own type.
fn clone_source_type(body: &Body, arg: ExprId) -> TypeId {
    let inner = match &body.exprs[arg].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr,
        } => *expr,
        _ => arg,
    };
    match &body.exprs[inner].kind {
        ExprKind::FieldAccess { expr, .. } => body.exprs[*expr].type_id,
        _ => body.exprs[inner].type_id,
    }
}
