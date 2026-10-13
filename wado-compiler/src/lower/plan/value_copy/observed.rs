//! The fields whose storage a reference may observe across an assignment the
//! assigning body cannot see (WEP 2026-10-13): a borrow of a place through the
//! field that outlives the expression taking it, or one lent to a call that may
//! write the field.
//!
//! A body that owns the root of a place answers for its own borrows from its
//! liveness. A place reached through a parameter or a reference is the caller's,
//! and so is any borrow of it, which is what this set stands for.

use super::funcset::FuncKeySet;
use super::modref::ModRef;
use super::retention::CallRetention;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::lower::plan::value_copy::place::field_owner;
use crate::tir::{
    FunctionRef, ResolvedType, StructDef, TirExpr, TirExprKind, TirField, TirPattern, TirStmt,
    TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// `(owner, field)` pairs some borrow may observe from outside the body that
/// assigns them.
#[derive(Default)]
pub struct ObservedFields(IndexSet<(TypeId, u32)>);

impl ObservedFields {
    #[must_use]
    pub fn contains(&self, owner: TypeId, field: u32) -> bool {
        self.0.contains(&(owner, field))
    }
}

pub fn compute(
    flat: &FlatPackage,
    retention: &CallRetention,
    mod_ref: &ModRef,
    ref_receiver_methods: &FuncKeySet,
) -> ObservedFields {
    let type_table = flat.type_table.borrow();
    let mut scan = Scan {
        type_table: &type_table,
        fields: flat
            .structs
            .iter()
            .map(|s| ((s.def, s.type_args.clone()), s.fields.clone()))
            .collect(),
        retention,
        mod_ref,
        ref_receiver_methods,
        lent: IndexSet::default(),
        observed: IndexSet::default(),
    };
    for func in &flat.functions {
        if let Some(body) = &func.borrow().body {
            scan.visit_block(body);
        }
    }
    ObservedFields(scan.observed)
}

struct Scan<'a> {
    type_table: &'a TypeTable,
    fields: IndexMap<(StructDef, Vec<TypeId>), Vec<TirField>>,
    retention: &'a CallRetention,
    mod_ref: &'a ModRef,
    ref_receiver_methods: &'a FuncKeySet,
    /// The borrows a call argument already answered for, by address, so the
    /// walk below them does not count them again as held.
    lent: IndexSet<*const TirExpr>,
    observed: IndexSet<(TypeId, u32)>,
}

impl Scan<'_> {
    /// The declared type of field `index` of struct `owner`.
    fn field_type_of(&self, owner: TypeId, index: u32) -> TypeId {
        let ResolvedType::Struct { def, type_args } = self.type_table.get(owner) else {
            unreachable!("a struct pattern matches a struct")
        };
        self.fields[&(*def, type_args.clone())][index as usize].type_id
    }

    /// The fields a place passes through, outermost first: `h.p.q` is
    /// `(H, p), (P, q)`.
    fn field_steps(&self, place: &TirExpr, out: &mut Vec<(TypeId, u32)>) {
        match &place.kind {
            TirExprKind::FieldAccess {
                expr, field_index, ..
            } => {
                self.field_steps(expr, out);
                out.push((field_owner(expr.type_id, self.type_table), *field_index));
            }
            TirExprKind::Index { expr, .. }
            | TirExprKind::Cast { expr, .. }
            | TirExprKind::VariantPayload { expr, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr,
            } => self.field_steps(expr, out),
            _ => {}
        }
    }

    /// A borrow of `place` lent to `func` at `position`, which keeps it past the
    /// call or not. Lent and not kept, it is observed only from the outermost
    /// field the callee may write, since only an assignment the call makes can
    /// meet it.
    fn lend(&mut self, place: &TirExpr, func: Option<&FunctionRef>, position: u32, call: &TirExpr) {
        let mut steps = Vec::new();
        self.field_steps(place, &mut steps);
        let Some(func) = func else {
            self.observed.extend(steps);
            return;
        };
        let holds = !matches!(
            self.type_table.get(call.type_id),
            ResolvedType::Primitive(_) | ResolvedType::Unit
        );
        let arity = match &call.kind {
            TirExprKind::Call { args, .. } => args.len(),
            _ => unreachable!("a lent borrow is a call's argument"),
        };
        if self.retention.of_call(func, arity, holds).keeps(position) {
            self.observed.extend(steps);
            return;
        }
        let writes = self.mod_ref.writes(&func.module_source, &func.name);
        if let Some(first) = steps
            .iter()
            .position(|(owner, field)| writes.may_write_field(*owner, *field))
        {
            self.observed.extend(steps[first..].iter().copied());
        }
    }

    /// A pattern binding a reference to a field of what it matches: the
    /// binding holds that field's storage.
    fn visit_binding_pattern(
        &mut self,
        pattern: &TirPattern,
        owner: TypeId,
        path: &mut Vec<(TypeId, u32)>,
    ) {
        match pattern {
            TirPattern::Binding { type_id, .. } => {
                if !path.is_empty()
                    && matches!(
                        self.type_table.get(*type_id),
                        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                    )
                {
                    self.observed.extend(path.iter().copied());
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
                let types = self.type_table.as_tuple(tuple).unwrap_or_default();
                for (index, element) in elements.iter().enumerate() {
                    let index = u32::try_from(index).expect("tuple arity");
                    path.push((tuple, index));
                    let element_type = types.get(index as usize).copied().unwrap_or(tuple);
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
}

impl TirRefVisitor for Scan<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::LetDestructure { pattern, value } = &stmt.kind
            && matches!(
                self.type_table.get(value.type_id),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        {
            self.visit_binding_pattern(pattern, value.type_id, &mut Vec::new());
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                for (position, arg) in args.iter().enumerate() {
                    let position = u32::try_from(position).expect("arity");
                    if let TirExprKind::Unary {
                        op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                        expr: place,
                    } = &arg.expr.kind
                    {
                        self.lend(place, Some(func), position, expr);
                        self.lent.insert(std::ptr::from_ref(&arg.expr));
                    }
                }
                // A `&self` / `&mut self` receiver is borrowed with no `&`.
                if let Some((receiver, _)) = expr.kind.call_receiver()
                    && self
                        .ref_receiver_methods
                        .contains(&func.module_source, &func.name)
                {
                    self.lend(receiver, Some(func), 0, expr);
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args {
                    if let TirExprKind::Unary {
                        op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                        expr: place,
                    } = &arg.kind
                    {
                        self.lend(place, None, 0, expr);
                        self.lent.insert(std::ptr::from_ref(arg));
                    }
                }
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } if !self.lent.contains(&std::ptr::from_ref(expr)) => {
                let mut steps = Vec::new();
                self.field_steps(place, &mut steps);
                self.observed.extend(steps);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                if matches!(
                    self.type_table.get(scrutinee.type_id),
                    ResolvedType::Ref(_) | ResolvedType::MutRef(_)
                ) {
                    for arm in arms {
                        let mut path = Vec::new();
                        self.visit_binding_pattern(&arm.pattern, scrutinee.type_id, &mut path);
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
