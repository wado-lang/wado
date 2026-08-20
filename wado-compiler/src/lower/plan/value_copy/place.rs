//! What an expression names, for every analysis in this module that asks.
//!
//! One resolver, and an answer that keeps "a value of its own" apart from "a
//! place this walk cannot follow": an analysis may ignore the first and must
//! not ignore the second.

use super::funcset::{FuncKeyMap, FuncKeySet};
use crate::hashmap::IndexMap;
use crate::tir::{
    ResolvedType, TirExpr, TirExprKind, TirFunction, TirPattern, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// One step of a projection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selector {
    /// A struct field, with the type carrying it — the identity an
    /// interprocedural consumer keys on, a local one ignores.
    Field {
        owner: TypeId,
        index: u32,
    },
    Variant(u32),
    Index,
}

impl Selector {
    #[must_use]
    pub fn field_index(self) -> Option<u32> {
        match self {
            Selector::Field { index, .. } => Some(index),
            Selector::Variant(_) | Selector::Index => None,
        }
    }
}

/// A storage location: a root local plus a root-first chain of projections.
/// `self.rows[0]` is `{ root: self, selectors: [Field(rows), Index] }`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    pub root: u32,
    pub selectors: Vec<Selector>,
}

impl Place {
    #[must_use]
    pub fn local(root: u32) -> Self {
        Self {
            root,
            selectors: Vec::new(),
        }
    }
}

/// What an expression names.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Names {
    /// Exactly this storage.
    Place(Place),
    /// A value of its own — a literal, an arithmetic result, a call that
    /// returns owned storage. Nothing else names it.
    Value,
    /// A place this walk cannot name. Every consumer owes this its
    /// conservative answer.
    Unknown,
}

/// What each local stands for, so a read or a write through a name reaches the
/// storage it was taken over. Covers every binder that hands out a name: a
/// borrow, a receiver-projecting accessor's result, a destructuring pattern,
/// and a reference re-seated by assignment.
#[derive(Default)]
pub struct Bindings {
    map: IndexMap<u32, Names>,
}

impl Bindings {
    fn get(&self, local: u32) -> Option<&Names> {
        self.map.get(&local)
    }

    fn set(&mut self, local: u32, names: Names) {
        self.map.insert(local, names);
    }
}

/// Which projection of its receiver each accessor returns: `list[i]` returns
/// the element, `self.get()` the field it borrows. A call with no entry returns
/// storage this walk cannot place.
pub type ReturnPaths = FuncKeyMap<Vec<Selector>>;

/// The projection each function returns out of its first parameter, for the
/// calls that name storage rather than build it.
#[must_use]
pub fn compute_return_paths(
    flat: &crate::flat_package::FlatPackage,
    type_table: &TypeTable,
    returns_owned: &FuncKeySet,
) -> ReturnPaths {
    let empty = ReturnPaths::default();
    let mut paths = ReturnPaths::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let (Some(body), Some(receiver)) = (&func.body, func.params.first()) else {
            continue;
        };
        if !is_reference(func.return_type, type_table) {
            continue;
        }
        let resolver = Resolver::new(&func, type_table, &empty, returns_owned);
        let mut returned = ReturnedPlace {
            resolver: &resolver,
            names: None,
        };
        returned.visit_block(body);
        if let Some(Names::Place(place)) = returned.names
            && place.root == receiver.local_index
        {
            paths.insert(
                func.module_source.clone(),
                func.name.clone(),
                place.selectors,
            );
        }
    }
    paths
}

/// The single place a body returns, or `None` where it returns more than one
/// shape — which is a shape this walk does not place.
struct ReturnedPlace<'r, 'a> {
    resolver: &'r Resolver<'a>,
    names: Option<Names>,
}

impl TirRefVisitor for ReturnedPlace<'_, '_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Return { value: Some(value) } = &stmt.kind {
            let names = self.resolver.names(value);
            self.names = match self.names.take() {
                None => Some(names),
                Some(seen) if seen == names => Some(seen),
                Some(_) => Some(Names::Unknown),
            };
        }
        self.walk_stmt(stmt);
    }
}

/// Resolve expressions of one function against what its locals stand for.
pub struct Resolver<'a> {
    type_table: &'a TypeTable,
    /// What each accessor returns out of its receiver.
    return_paths: &'a ReturnPaths,
    /// Callees whose result is storage of its own. Every other call hands back
    /// something this walk cannot place — a projection of an argument it does
    /// not follow — and says so.
    returns_owned: &'a FuncKeySet,
    /// Parameters naming storage the caller lent, by the type lent. The only
    /// roots a write in this body reaches out through.
    lent: IndexMap<u32, TypeId>,
    bindings: Bindings,
}

impl<'a> Resolver<'a> {
    /// Walk `func` once, recording what each of its locals stands for. A
    /// parameter of reference shape stands for storage its caller lent, which
    /// is the one root a write can reach out through.
    #[must_use]
    pub fn new(
        func: &TirFunction,
        type_table: &'a TypeTable,
        return_paths: &'a ReturnPaths,
        returns_owned: &'a FuncKeySet,
    ) -> Self {
        let mut resolver = Self {
            type_table,
            return_paths,
            returns_owned,
            lent: IndexMap::default(),
            bindings: Bindings::default(),
        };
        for param in &func.params {
            if param.is_mut_ref || is_reference(param.type_id, type_table) {
                resolver
                    .lent
                    .insert(param.local_index, type_table.peel_refs(param.type_id));
            }
        }
        if let Some(body) = &func.body {
            let mut collector = BindingCollector {
                resolver: &mut resolver,
            };
            collector.visit_block(body);
        }
        resolver
    }

    /// The type `local` was lent, for a parameter naming a caller's storage.
    #[must_use]
    pub fn lent(&self, local: u32) -> Option<TypeId> {
        self.lent.get(&local).copied()
    }

    /// What `expr` names. Total over the expression kinds: a shape with no arm
    /// of its own is [`Names::Unknown`], never silently nothing.
    #[must_use]
    pub fn names(&self, expr: &TirExpr) -> Names {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self
                .bindings
                .get(*index)
                .cloned()
                .unwrap_or(Names::Place(Place::local(*index))),
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => self.project(
                inner,
                Selector::Field {
                    owner: self.type_table.peel_refs(inner.type_id),
                    index: *field_index,
                },
            ),
            TirExprKind::VariantPayload {
                expr: inner,
                case_index,
                ..
            } => self.project(inner, Selector::Variant(*case_index)),
            TirExprKind::Index { expr: inner, .. } => self.project(inner, Selector::Index),
            // A cast converts nothing about storage, and a borrow names what it
            // is taken over.
            TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: inner,
            } => self.names(inner),
            // Reading through a reference reaches what the reference names.
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => match self.names(inner) {
                Names::Place(p) => Names::Place(p),
                Names::Value | Names::Unknown => Names::Unknown,
            },
            // A call naming storage names its receiver's, at the projection it
            // returns. One that builds its result names a value of its own; one
            // that hands back storage this walk cannot place names nothing it
            // can answer for.
            TirExprKind::Call { func, args, .. } => {
                match self.return_paths.get(&func.module_source, &func.name) {
                    Some(selectors) => match args.first().map(|r| self.names(&r.expr)) {
                        Some(Names::Place(mut place)) => {
                            place.selectors.extend(selectors.iter().copied());
                            Names::Place(place)
                        }
                        Some(other) => other,
                        None => Names::Unknown,
                    },
                    None if self.returns_owned.contains(&func.module_source, &func.name) => {
                        Names::Value
                    }
                    None => Names::Unknown,
                }
            }
            // Shapes that build their own storage.
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::VariantConstruct { .. }
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::Binary { .. } => Names::Value,
            _ => Names::Unknown,
        }
    }

    fn project(&self, inner: &TirExpr, selector: Selector) -> Names {
        match self.names(inner) {
            Names::Place(mut place) => {
                place.selectors.push(selector);
                Names::Place(place)
            }
            // A projection out of a fresh value is storage of that value's own.
            Names::Value => Names::Value,
            Names::Unknown => Names::Unknown,
        }
    }

    fn bind_pattern(&mut self, pattern: &TirPattern, base: &Names) {
        match pattern {
            TirPattern::Binding { local_index, .. } => {
                self.bindings.set(*local_index, base.clone())
            }
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                let owner = self.type_table.peel_refs(*struct_type);
                for field in fields {
                    let nested = match base {
                        Names::Place(place) => {
                            let mut place = place.clone();
                            place.selectors.push(Selector::Field {
                                owner,
                                index: field.field_index,
                            });
                            Names::Place(place)
                        }
                        Names::Value => Names::Value,
                        Names::Unknown => Names::Unknown,
                    };
                    self.bind_pattern(&field.pattern, &nested);
                }
            }
            // An element, a payload and an alternative name storage inside the
            // place destructured, which that place answers for.
            TirPattern::Tuple(sub, _)
            | TirPattern::Variant { bindings: sub, .. }
            | TirPattern::Or(sub) => {
                for pattern in sub {
                    self.bind_pattern(pattern, base);
                }
            }
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {}
        }
    }
}

/// A name for storage someone else owns, as the type table spells it here —
/// after `boxing::prepare_types`, where a `&primitive` reads as its `Box<T>`.
#[must_use]
pub fn is_reference(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_)
    ) || type_table.is_reference_shaped(type_id)
}

/// A handle a callee can write through. A shared `&` cannot be; a box carries
/// either spelling and answers for both.
#[must_use]
pub fn could_write_through(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::MutRef(_) => true,
        ResolvedType::Ref(_) => false,
        _ => type_table.is_reference_shaped(type_id),
    }
}

/// Records what every binder in a body hands out.
struct BindingCollector<'r, 'a> {
    resolver: &'r mut Resolver<'a>,
}

impl TirRefVisitor for BindingCollector<'_, '_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                // A binding of reference shape stands for what its value names;
                // one out of a value owns storage of its own.
                let names = if is_reference(value.type_id, self.resolver.type_table) {
                    self.resolver.names(value)
                } else {
                    Names::Value
                };
                self.resolver.bindings.set(*local_index, names);
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                let base = self.resolver.names(value);
                self.resolver.bind_pattern(pattern, &base);
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // Re-seating a reference makes it stand for what it now names.
            TirExprKind::Assign { target, value } => {
                if let TirExprKind::Local { index, .. } = &target.kind
                    && is_reference(target.type_id, self.resolver.type_table)
                {
                    let names = self.resolver.names(value);
                    self.resolver.bindings.set(*index, names);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let base = self.resolver.names(scrutinee);
                for arm in arms {
                    self.resolver.bind_pattern(&arm.pattern, &base);
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}
