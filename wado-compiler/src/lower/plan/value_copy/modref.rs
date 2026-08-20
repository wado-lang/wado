//! What a call writes into its receiver, as fields of the receiver's type. A
//! caller reading one field while a call writes another needs no defensive
//! copy; today's answer — a call writes the whole receiver — is what a callee
//! this analysis cannot read still gets.

use super::funcset::{FuncKeyMap, FuncKeySet};
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{
    TirExpr, TirExprKind, TirFunction, TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// A handle a callee can write through. A shared `&` cannot be, so handing one
/// to a callee this analysis cannot read tells it nothing.
fn is_mut_reference(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(type_id), crate::tir::ResolvedType::MutRef(_))
}

/// A name for storage someone else owns.
fn is_reference(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
    )
}

/// The fields one function writes, by the type carrying each.
#[derive(Default, Clone, PartialEq)]
pub struct Writes {
    fields: IndexSet<(TypeId, u32)>,
    /// Types written whole: a `*p = v` through a reference parameter replaces
    /// every field of what the caller lent.
    whole: IndexSet<TypeId>,
    /// A write this analysis cannot attribute: a place it does not recognise,
    /// or a callee it cannot read that was handed a reference. Everything the
    /// caller can name is written.
    opaque: bool,
}

impl Writes {
    /// Whether a call may write anything the caller cannot rule out.
    #[must_use]
    pub fn is_opaque(&self) -> bool {
        self.opaque
    }

    /// Whether a value of `owner` is written past any one field.
    #[must_use]
    pub fn writes_whole(&self, owner: TypeId) -> bool {
        self.whole.contains(&owner)
    }

    /// The fields of `owner` this writes, for a caller re-rooting them at its
    /// own receiver.
    pub fn fields_of(&self, owner: TypeId) -> impl Iterator<Item = u32> + '_ {
        self.fields
            .iter()
            .filter(move |(ty, _)| *ty == owner)
            .map(|(_, field)| *field)
    }
}

/// Every function's writes, closed over the call graph.
#[derive(Default)]
pub struct ModRef {
    per_func: FuncKeyMap<Writes>,
}

impl ModRef {
    /// What calling `(module, name)` writes. An unknown callee writes anything.
    #[must_use]
    pub fn writes(&self, module: &ModuleSource, name: &str) -> Writes {
        self.per_func.get(module, name).cloned().unwrap_or(Writes {
            opaque: true,
            ..Writes::default()
        })
    }
}

/// Collect each body's own writes, then close over the call graph: a caller
/// writes what its callees write.
#[must_use]
pub fn compute_mod_ref(flat: &FlatPackage, returns_receiver_alias: &FuncKeySet) -> ModRef {
    let type_table = flat.type_table.borrow();
    let mut direct: Vec<(ModuleSource, String, Writes, Vec<(ModuleSource, String)>)> = Vec::new();
    // A body this scan reads. One without — an import, a builtin — reaches the
    // caller only through what it is handed, which the call site answers for.
    let mut defined = FuncKeySet::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if func.body.is_some() {
            defined.insert(func.module_source.clone(), func.name.clone());
        }
    }
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let (writes, callees) = scan(&func, &type_table, &defined, returns_receiver_alias);
        direct.push((
            func.module_source.clone(),
            func.name.clone(),
            writes,
            callees,
        ));
    }

    let mut per_func: FuncKeyMap<Writes> = FuncKeyMap::default();
    for (module, name, writes, _) in &direct {
        per_func.insert(module.clone(), name.clone(), writes.clone());
    }
    let mut changed = true;
    while changed {
        changed = false;
        for (module, name, _, callees) in &direct {
            let mut merged = per_func.get(module, name).cloned().unwrap_or_default();
            for (cm, cn) in callees {
                let callee = per_func.get(cm, cn).cloned().unwrap_or_default();
                merged.opaque |= callee.opaque;
                for f in callee.fields {
                    merged.fields.insert(f);
                }
                for t in callee.whole {
                    merged.whole.insert(t);
                }
            }
            if per_func.get(module, name) != Some(&merged) {
                per_func.insert(module.clone(), name.clone(), merged);
                changed = true;
            }
        }
    }
    ModRef { per_func }
}

fn scan(
    func: &TirFunction,
    type_table: &TypeTable,
    defined: &FuncKeySet,
    returns_receiver_alias: &FuncKeySet,
) -> (Writes, Vec<(ModuleSource, String)>) {
    let mut walker = Walker {
        type_table,
        defined,
        returns_receiver_alias,
        places: IndexMap::default(),
        writes: Writes::default(),
        callees: Vec::new(),
    };
    let Some(body) = &func.body else {
        // No body to read: whatever it does, it does where nothing can see.
        return (
            Writes {
                opaque: true,
                ..Writes::default()
            },
            Vec::new(),
        );
    };
    walker.visit_block(body);
    (walker.writes, walker.callees)
}

/// A local naming storage inside another place: what a borrow or a destructuring
/// pattern binds. A write through one is a write to the place it names.
#[derive(Clone, Default)]
struct Place {
    fields: Vec<(TypeId, u32)>,
}

struct Walker<'a> {
    type_table: &'a TypeTable,
    defined: &'a FuncKeySet,
    /// Accessors returning a projection of their receiver — `list[i]` is one,
    /// so a write through the result is a write inside the receiver.
    returns_receiver_alias: &'a FuncKeySet,
    places: IndexMap<u32, Place>,
    writes: Writes,
    callees: Vec<(ModuleSource, String)>,
}

impl Walker<'_> {
    /// Mark every field a write through `place` reaches. A shape this does not
    /// recognise gives up precision for the whole function rather than answer
    /// for storage it cannot name.
    fn mark(&mut self, place: &TirExpr) {
        match &place.kind {
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                let owner = self.type_table.peel_refs(inner.type_id);
                self.writes.fields.insert((owner, *field_index));
                self.mark_base(inner);
            }
            // An element or a pointee is storage inside the place it is taken
            // over, which that place's own fields answer for.
            TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Deref | TirUnaryOp::MutRef | TirUnaryOp::Ref,
                expr: inner,
            }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => self.mark(inner),
            // A local naming another place writes that place. A reference
            // names storage this function was lent, replaced whole; a local of
            // its own is storage no caller can see.
            TirExprKind::Local { index, .. } => match self.places.get(index).cloned() {
                Some(named) => {
                    for field in named.fields {
                        self.writes.fields.insert(field);
                    }
                }
                None if is_reference(place.type_id, self.type_table) => {
                    self.writes
                        .whole
                        .insert(self.type_table.peel_refs(place.type_id));
                }
                None => {}
            },
            // An accessor returning a projection of its receiver names storage
            // inside it, so the receiver answers for the write.
            TirExprKind::Call { func, args, .. }
                if self
                    .returns_receiver_alias
                    .contains(&func.module_source, &func.name) =>
            {
                match args.first() {
                    Some(receiver) => self.mark(&receiver.expr),
                    None => self.writes.opaque = true,
                }
            }
            _ => self.writes.opaque = true,
        }
    }

    /// Mark only what a place names through this function's own links: the
    /// fields a `FieldAccess` spells, and the place a destructured binding or a
    /// borrow stands for. A bare reference names storage the call graph answers
    /// for, so it marks nothing.
    fn mark_linked(&mut self, place: &TirExpr) {
        match &place.kind {
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                let owner = self.type_table.peel_refs(inner.type_id);
                self.writes.fields.insert((owner, *field_index));
                self.mark_linked(inner);
            }
            TirExprKind::Index { expr: inner, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Deref | TirUnaryOp::MutRef | TirUnaryOp::Ref,
                expr: inner,
            }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => self.mark_linked(inner),
            TirExprKind::Local { index, .. } => {
                if let Some(named) = self.places.get(index).cloned() {
                    for field in named.fields {
                        self.writes.fields.insert(field);
                    }
                }
            }
            _ => {}
        }
    }

    /// Mark the fields on the way to a place, without marking the place itself
    /// again.
    fn mark_base(&mut self, place: &TirExpr) {
        match &place.kind {
            TirExprKind::Local { .. } => {}
            _ => self.mark(place),
        }
    }

    /// Record what a pattern binds out of `scrutinee`, so a write through one of
    /// its bindings names the field it was taken from.
    fn bind_pattern(&mut self, pattern: &TirPattern, base: Option<Place>) {
        let Some(base) = base else { return };
        match pattern {
            TirPattern::Binding { local_index, .. } => {
                self.places.insert(*local_index, base);
            }
            // The pattern names the type it destructures, so a nested one keys
            // its own fields without asking what a field's type is.
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                let owner = self.type_table.peel_refs(*struct_type);
                for field in fields {
                    let mut nested = base.clone();
                    nested.fields.push((owner, field.field_index));
                    self.bind_pattern(&field.pattern, Some(nested));
                }
            }
            // An element, a payload and an alternative all name storage inside
            // the place destructured, which that place's own fields answer for.
            TirPattern::Tuple(sub, _)
            | TirPattern::Variant { bindings: sub, .. }
            | TirPattern::Or(sub) => {
                for pattern in sub {
                    self.bind_pattern(pattern, Some(base.clone()));
                }
            }
            TirPattern::ConstantValue { .. } => {}
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::Range { .. } => {}
        }
    }

    /// The place `expr` names, when it is one this walk follows.
    fn place_of(&self, expr: &TirExpr) -> Option<Place> {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                Some(self.places.get(index).cloned().unwrap_or_default())
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                let mut base = self.place_of(inner)?;
                let owner = self.type_table.peel_refs(inner.type_id);
                base.fields.push((owner, *field_index));
                Some(base)
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref,
                expr: inner,
            }
            | TirExprKind::Cast { expr: inner, .. } => self.place_of(inner),
            _ => None,
        }
    }
}

impl TirRefVisitor for Walker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::LetDestructure { pattern, value, .. } = &stmt.kind {
            let base = self.place_of(value);
            self.bind_pattern(pattern, base);
        }
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: place,
            } = &value.kind
            && let Some(named) = self.place_of(place)
        {
            self.places.insert(*local_index, named);
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Assign { target, .. } => self.mark(target),
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let base = self.place_of(scrutinee);
                for arm in arms {
                    self.bind_pattern(&arm.pattern, base.clone());
                }
            }
            // The callee's own writes arrive through the call graph; only its
            // identity is needed here. One this analysis cannot read reaches
            // the caller's storage only through a reference it is handed — a
            // by-value call, `panic` among them, writes nothing anyone here can
            // name.
            TirExprKind::Call { func, args, .. } => {
                let known = self.defined.contains(&func.module_source, &func.name);
                if known {
                    self.callees
                        .push((func.module_source.clone(), func.name.clone()));
                }
                for arg in args
                    .iter()
                    .filter(|a| is_mut_reference(a.expr.type_id, self.type_table))
                {
                    // What the argument names *here*: a callee writing its own
                    // parameter writes the place this body lent it.
                    self.mark_linked(&arg.expr);
                    // A callee this analysis cannot read reaches only what it is
                    // handed, and only through a handle it can write.
                    if !known {
                        self.writes
                            .whole
                            .insert(self.type_table.peel_refs(arg.expr.type_id));
                    }
                }
            }
            // A closure reaches this function's storage only through what it is
            // handed and what it captured, and a capture of a place is a `&mut`
            // where the closure was built — marked there, in the body that lent
            // the storage.
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args
                    .iter()
                    .filter(|a| is_mut_reference(a.type_id, self.type_table))
                {
                    self.mark_linked(arg);
                    self.writes
                        .whole
                        .insert(self.type_table.peel_refs(arg.type_id));
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
