//! Materialize `NirExprKind::ArrayLiteral` from the `SequenceLiteralBuilder`
//! coercion of an array literal.
//!
//! An array literal `[e0, e1, …] as Array<T>` is lowered during elaboration
//! (see `elaborator::reify::reify_sequence_coercion`) into a `__seq_lit:`
//! labeled block over a fresh builder local:
//!
//! ```text
//! __seq_lit: {
//!     let mut __b: Array<T> = Array<T>^SequenceLiteralBuilder::new_literal(N);
//!     __b.push_literal(e0);
//!     __b.push_literal(e1);
//!     …
//!     break __seq_lit: __b.build();
//! }
//! ```
//!
//! This pass recognizes that shape and rewrites the whole labeled block to a
//! single `NirExprKind::ArrayLiteral { elements: [e0, e1, …] }`, giving a
//! constant array the same first-class, analyzable value form that
//! `StructLiteral` / `TupleLiteral` already have. `wir_build` lowers the
//! result to `WirInstr::ArrayNewFixed` — the same instruction the (now
//! retired) WIR-level `collapse_array_push_sequences` used to produce, so WIR
//! output is unchanged while NIR analysis (cse, const-folding,
//! bounds-check elimination, constant globalization) gains visibility.
//!
//! The builder methods are identified by their `SequenceLiteralBuilder` trait
//! and method names (`new_literal` / `push_literal` / `build`), not by
//! canonical paths, mirroring how `string_push` identifies its methods. The
//! receivers of `push_literal` / `build` are taken through `&mut self` / `&self`,
//! so the matcher peels the surrounding `&` / `&mut`.
//!
//! Runs inside the optimizer fixpoint loop, before `inline`: the inliner
//! expands the builder calls into raw `Array::push` sequences, after which
//! this trait-method-keyed rewrite no longer fires. Running early also lets
//! `cse` / `const_fold` in the same loop see the normalized literal.

use crate::nir::{NirExpr, NirExprKind, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirOptVisitor, opt_walk_expr};
use crate::tir::TypeTable;

/// The synthetic label the elaborator gives the `SequenceLiteralBuilder`
/// coercion block (see `elaborator::reify` / `elaborator::coercion`).
const SEQ_LIT_LABEL: &str = "__seq_lit";

/// The trait whose `new_literal` / `push_literal` / `build` methods implement
/// array-literal construction.
const SEQUENCE_LITERAL_BUILDER: &str = "SequenceLiteralBuilder";

pub fn collapse_array_literals(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let mut visitor = ArrayLiteralVisitor {
        type_table: &type_table,
    };
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = &mut func.body {
            changed |= visitor.visit_block(body);
        }
    }
    changed
}

struct ArrayLiteralVisitor<'a> {
    type_table: &'a TypeTable,
}

impl NirOptVisitor for ArrayLiteralVisitor<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        // Recurse first so a literal nested inside another literal's element
        // (e.g. `[[1, 2], [3, 4]]`) collapses bottom-up.
        let mut changed = opt_walk_expr(self, expr);
        // `SequenceLiteralBuilder` is user-implementable, so the `__seq_lit:`
        // shape also covers custom builder targets. `ArrayLiteral` is the
        // canonical form for `Array<T>` only — `wir_build` lowers it to the
        // `Array<T>` `{ repr, used }` struct — so gate on the result type
        // being `Array<T>`. Other builder targets keep their imperative form.
        if self.type_table.as_array(expr.type_id).is_some()
            && let Some(elements) = try_match_seq_lit_block(&expr.kind)
        {
            expr.kind = NirExprKind::ArrayLiteral { elements };
            changed = true;
        }
        changed
    }
}

/// If `kind` is the `__seq_lit:` builder block, extract its element
/// expressions in push order. Returns `None` for any deviation from the
/// canonical shape — the local is then a genuinely growable array, not a
/// literal, and is left untouched.
fn try_match_seq_lit_block(kind: &NirExprKind) -> Option<Vec<NirExpr>> {
    let NirExprKind::LabeledBlock { label, block, .. } = kind else {
        return None;
    };
    if label != SEQ_LIT_LABEL {
        return None;
    }

    // Canonical shape: `let __b = new_literal(N)`, then N × `__b.push_literal(e)`,
    // then `break __seq_lit: __b.build()`.
    let (first, rest) = block.stmts.split_first()?;
    let (last, pushes) = rest.split_last()?;

    // First statement binds the builder local to `new_literal(capacity)`.
    let NirStmtKind::Let {
        local_index: builder_local,
        value,
        ..
    } = &first.kind
    else {
        return None;
    };
    if !is_builder_call(&value.kind, "new_literal") {
        return None;
    }
    let builder_local = *builder_local;

    // Trailing `break __seq_lit: __b.build()`.
    let NirStmtKind::Break {
        label: Some(break_label),
        value: Some(build_expr),
    } = &last.kind
    else {
        return None;
    };
    if break_label != SEQ_LIT_LABEL
        || !is_builder_method_on(&build_expr.kind, "build", builder_local)
    {
        return None;
    }

    // Every middle statement is `__b.push_literal(elem)`.
    let mut elements = Vec::with_capacity(pushes.len());
    for stmt in pushes {
        let NirStmtKind::Expr(call) = &stmt.kind else {
            return None;
        };
        let element = match_push_literal(&call.kind, builder_local)?;
        elements.push(element.clone());
    }

    Some(elements)
}

/// True if `kind` is a call to the builder's static `new_literal` associated
/// method. The capacity argument is ignored — the element count determines
/// the literal's length, and the WIR builder re-derives capacity from the
/// elements.
fn is_builder_call(kind: &NirExprKind, method: &str) -> bool {
    match kind {
        NirExprKind::Call { func, .. } | NirExprKind::MethodCall { func, .. } => {
            is_builder_method(func.method_info.as_ref(), method)
        }
        _ => false,
    }
}

/// True if `kind` is a `MethodCall` of the builder's `method` whose receiver
/// is (a reference to) `Local(builder_local)`.
fn is_builder_method_on(kind: &NirExprKind, method: &str, builder_local: u32) -> bool {
    let NirExprKind::MethodCall { receiver, func, .. } = kind else {
        return false;
    };
    is_builder_method(func.method_info.as_ref(), method)
        && is_builder_receiver(receiver, builder_local)
}

/// Match `__b.push_literal(elem)` on the builder local and return `elem`.
fn match_push_literal(kind: &NirExprKind, builder_local: u32) -> Option<&NirExpr> {
    let NirExprKind::MethodCall {
        receiver,
        func,
        args,
        ..
    } = kind
    else {
        return None;
    };
    if !is_builder_method(func.method_info.as_ref(), "push_literal")
        || !is_builder_receiver(receiver, builder_local)
        || args.len() != 1
    {
        return None;
    }
    Some(&args[0].expr)
}

fn is_builder_method(method_info: Option<&crate::name::LocalMethodName>, method: &str) -> bool {
    method_info.is_some_and(|info| {
        info.method_name == method
            && info.base_trait_name.as_deref() == Some(SEQUENCE_LITERAL_BUILDER)
    })
}

/// True if `expr` refers to `Local(local_index)`, peeling the `&` / `&mut`
/// the builder methods take their receiver through (`push_literal(&mut self)`,
/// `build(&self)`).
fn is_builder_receiver(expr: &NirExpr, local_index: u32) -> bool {
    match &expr.kind {
        NirExprKind::Local { index, .. } => *index == local_index,
        NirExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => is_builder_receiver(inner, local_index),
        _ => false,
    }
}
