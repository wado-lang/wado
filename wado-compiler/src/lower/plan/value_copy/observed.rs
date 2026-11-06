//! The fields whose storage a reference may observe across an assignment the
//! assigning body cannot see (WEP 2026-10-13).
//!
//! A body answers for its own references from its liveness, whatever the
//! place's root. What it cannot see is a reference another body holds while it
//! runs, and that is this set: a field a reference is taken through, where the
//! reference
//!
//! - outlives the body taking it: returned, stored, or kept by a callee where
//!   the body cannot name it;
//! - is held in one of that body's locals while it makes a call that may write
//!   the field;
//! - is handed to a call that may write the field.

use super::funcset::FuncKeySet;
use super::modref::ModRef;
use super::place::field_owner;
use super::reach::{ClosureCaptures, StorageReach};
use super::retention::{CallRetention, RESULT};
use super::{is_reference_type, is_scalar_type};
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{
    FunctionRef, ResolvedType, StructFields, TirExpr, TirExprKind, TirPattern, TirStmt,
    TirStmtKind, TirUnaryOp, TypeId, struct_fields,
};
use crate::tir_visitor::TirRefVisitor;
use crate::type_table::TypeTable;

/// `(owner, field)` pairs some reference may observe from outside the body that
/// assigns them.
#[derive(Default)]
pub struct ObservedFields(IndexSet<(TypeId, u32)>);

impl ObservedFields {
    #[must_use]
    pub fn contains(&self, owner: TypeId, field: u32) -> bool {
        self.0.contains(&(owner, field))
    }
}

/// The fields a place passes through, outermost first: `h.p.q` is
/// `(H, p), (P, q)`.
type Steps = Vec<(TypeId, u32)>;

pub fn compute(
    flat: &FlatPackage,
    retention: &CallRetention,
    mod_ref: &ModRef,
    ref_receiver_methods: &FuncKeySet,
    closure_captures: &ClosureCaptures,
) -> ObservedFields {
    let type_table = flat.type_table.borrow();
    let fields = struct_fields(&flat.structs);
    let reach = StorageReach::new(&type_table, &fields, closure_captures);
    let mut observed = IndexSet::default();
    for func in &flat.functions {
        let func = func.borrow();
        let Some(body) = &func.body else {
            continue;
        };
        let mut scan = Scan {
            type_table: &type_table,
            fields: &fields,
            reach: &reach,
            retention,
            mod_ref,
            ref_receiver_methods,
            handed: IndexSet::default(),
            frame_bound: IndexSet::default(),
            escaping: Vec::new(),
            held: Vec::new(),
            callees: Vec::new(),
            indirect: false,
        };
        scan.visit_block(body);
        scan.finish(&mut observed);
    }
    ObservedFields(observed)
}

struct Scan<'a> {
    type_table: &'a TypeTable,
    fields: &'a StructFields,
    reach: &'a StorageReach<'a>,
    retention: &'a CallRetention,
    mod_ref: &'a ModRef,
    ref_receiver_methods: &'a FuncKeySet,
    /// The references a call argument already answered for, by address, so the
    /// walk below them does not count them again.
    handed: IndexSet<*const TirExpr>,
    /// Calls whose result a local of this body holds, by address.
    frame_bound: IndexSet<*const TirExpr>,
    escaping: Vec<Steps>,
    held: Vec<Steps>,
    callees: Vec<FunctionRef>,
    indirect: bool,
}

impl Scan<'_> {
    fn finish(self, observed: &mut IndexSet<(TypeId, u32)>) {
        for steps in self.escaping {
            observed.extend(steps);
        }
        for steps in self.held {
            let written = |&(owner, field): &(TypeId, u32)| {
                self.indirect
                    || self.callees.iter().any(|c| {
                        self.mod_ref
                            .may_write_field(&c.module_source, &c.name, owner, field)
                    })
            };
            if let Some(first) = steps.iter().position(written) {
                observed.extend(steps[first..].iter().copied());
            }
        }
    }

    /// The declared type of field `index` of struct `owner`.
    fn field_type_of(&self, owner: TypeId, index: u32) -> TypeId {
        let ResolvedType::Struct { def, type_args } = self.type_table.get(owner) else {
            unreachable!("a struct pattern matches a struct")
        };
        self.fields[&(*def, type_args.clone())][index as usize].type_id
    }

    fn field_steps(&self, place: &TirExpr, out: &mut Steps) {
        match &place.kind {
            TirExprKind::FieldAccess {
                expr, field_index, ..
            } => {
                self.field_steps(expr, out);
                out.push((field_owner(expr.type_id, self.type_table), *field_index));
            }
            // `&*&place` names `place`.
            TirExprKind::Index { expr, .. }
            | TirExprKind::Cast { expr, .. }
            | TirExprKind::VariantPayload { expr, .. }
            | TirExprKind::Unary { expr, .. } => self.field_steps(expr, out),
            _ => {}
        }
    }

    /// The steps `place` passes through, its inner references answered by the
    /// reference to `place` itself.
    fn steps(&mut self, place: &TirExpr) -> Steps {
        let mut out = Vec::new();
        self.field_steps(place, &mut out);
        self.mark_inner_refs(place);
        out
    }

    fn mark_inner_refs(&mut self, place: &TirExpr) {
        match &place.kind {
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr,
            } => {
                self.handed.insert(std::ptr::from_ref(place));
                self.mark_inner_refs(expr);
            }
            TirExprKind::FieldAccess { expr, .. }
            | TirExprKind::Index { expr, .. }
            | TirExprKind::Cast { expr, .. }
            | TirExprKind::VariantPayload { expr, .. }
            | TirExprKind::Unary { expr, .. } => self.mark_inner_refs(expr),
            _ => {}
        }
    }

    /// A reference to `place` handed to `func` at `position` of `call`. Kept only in
    /// the result, it is held where the result is: in this body's local for a
    /// frame-bound call, anywhere otherwise. Not kept, it is observed only from
    /// the outermost field the callee may write, since only an assignment the
    /// call makes can meet it.
    fn hand_ref(&mut self, place: &TirExpr, func: &FunctionRef, position: u32, call: &TirExpr) {
        let steps = self.steps(place);
        let TirExprKind::Call { args, .. } = &call.kind else {
            unreachable!("a handed reference is a call's argument")
        };
        let retained = self.retention.of_call(
            func,
            args.len(),
            !is_scalar_type(call.type_id, self.type_table),
        );
        if retained.keeps(position) {
            let into_result_only = retained
                .destinations(position)
                .is_some_and(|d| !d.is_empty() && d.iter().all(|&p| p == RESULT));
            if !into_result_only {
                self.escaping.push(steps);
                return;
            }
            // A result that cannot hold the place's storage keeps none of it.
            if self.reach.reaches(call.type_id, place.type_id, true) {
                if self.frame_bound.contains(&std::ptr::from_ref(call)) {
                    self.held.push(steps);
                } else {
                    self.escaping.push(steps);
                }
                return;
            }
        }
        if let Some(first) = steps.iter().position(|&(owner, field)| {
            self.mod_ref
                .may_write_field(&func.module_source, &func.name, owner, field)
        }) {
            self.escaping.push(steps[first..].to_vec());
        }
    }

    /// A pattern binding a reference to a field of what it matches: the
    /// binding, a local, holds that field's storage.
    fn visit_binding_pattern(&mut self, pattern: &TirPattern, owner: TypeId, path: &mut Steps) {
        match pattern {
            TirPattern::Binding { type_id, .. } => {
                if !path.is_empty() && is_reference_type(*type_id, self.type_table) {
                    self.held.push(path.clone());
                }
            }
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                let owner = field_owner(*struct_type, self.type_table);
                for field in fields {
                    path.push((owner, field.field_index));
                    let field_type = self.field_type_of(owner, field.field_index);
                    self.visit_binding_pattern(&field.pattern, field_type, path);
                    path.pop();
                }
            }
            TirPattern::Tuple(elements, _) => {
                let tuple = field_owner(owner, self.type_table);
                let types = self
                    .type_table
                    .as_tuple(tuple)
                    .expect("a tuple pattern matches a tuple");
                for (index, (element, &element_type)) in elements.iter().zip(&types).enumerate() {
                    let index = u32::try_from(index).expect("tuple arity");
                    path.push((tuple, index));
                    self.visit_binding_pattern(element, element_type, path);
                    path.pop();
                }
            }
            TirPattern::Or(alternatives) => {
                for alternative in alternatives {
                    self.visit_binding_pattern(alternative, owner, path);
                }
            }
            TirPattern::Variant {
                bindings,
                payload_type,
                ..
            } => {
                for binding in bindings {
                    self.visit_binding_pattern(binding, *payload_type, path);
                }
            }
            _ => {}
        }
    }

    /// A value a local of this body takes: a reference there is held, and so is
    /// one a call keeps in the result the local takes.
    fn bind_local(&mut self, value: &TirExpr) {
        let mut value = value;
        while let TirExprKind::Cast { expr, .. } = &value.kind {
            value = expr;
        }
        if let Some(place) = referenced_place(value) {
            let steps = self.steps(place);
            self.held.push(steps);
            self.handed.insert(std::ptr::from_ref(value));
        } else if matches!(value.kind, TirExprKind::Call { .. }) {
            self.frame_bound.insert(std::ptr::from_ref(value));
        }
    }
}

impl TirRefVisitor for Scan<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => self.bind_local(value),
            TirStmtKind::Expr(TirExpr {
                kind: TirExprKind::Assign { target, value },
                ..
            }) if matches!(target.kind, TirExprKind::Local { .. }) => self.bind_local(value),
            TirStmtKind::LetDestructure { pattern, value }
                if is_reference_type(value.type_id, self.type_table) =>
            {
                self.visit_binding_pattern(pattern, value.type_id, &mut Vec::new());
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                self.callees.push((**func).clone());
                for (position, arg) in args.iter().enumerate() {
                    let position = u32::try_from(position).expect("arity");
                    if let Some(place) = referenced_place(&arg.expr) {
                        self.hand_ref(place, func, position, expr);
                        self.handed.insert(std::ptr::from_ref(&arg.expr));
                    }
                }
                // A `&self` / `&mut self` receiver is referenced with no `&`.
                if let Some((receiver, _)) = expr.kind.call_receiver()
                    && self
                        .ref_receiver_methods
                        .contains(&func.module_source, &func.name)
                {
                    self.hand_ref(receiver, func, 0, expr);
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                self.indirect = true;
                for arg in args {
                    if let Some(place) = referenced_place(arg) {
                        let steps = self.steps(place);
                        self.escaping.push(steps);
                        self.handed.insert(std::ptr::from_ref(arg));
                    }
                }
            }
            TirExprKind::CmRawCall { .. } => self.indirect = true,
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } if !self.handed.contains(&std::ptr::from_ref(expr)) => {
                let steps = self.steps(place);
                self.escaping.push(steps);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                if is_reference_type(scrutinee.type_id, self.type_table) {
                    for arm in arms {
                        self.visit_binding_pattern(
                            &arm.pattern,
                            scrutinee.type_id,
                            &mut Vec::new(),
                        );
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// The place a `&place` or `&mut place` argument references.
fn referenced_place(expr: &TirExpr) -> Option<&TirExpr> {
    match &expr.kind {
        TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: place,
        } => Some(place),
        _ => None,
    }
}
