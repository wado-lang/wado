//! What an expression names, for every analysis in this module that asks. The
//! answer keeps "a value of its own" apart from "a place this walk cannot
//! follow": an analysis may ignore the first and must not ignore the second.

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
    /// A struct field, with the type carrying it: what an interprocedural
    /// consumer keys on.
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
    /// The chain read a `&` / `&mut` field, so the root only borrows what it
    /// ends at. [`Resolver::names`] must set it as it walks — it resolves
    /// through a local binding and through a callee's [`ReturnPath`], which a
    /// second walk over the expression's syntax would follow neither of.
    pub through_borrow: bool,
}

impl Place {
    #[must_use]
    pub fn local(root: u32) -> Self {
        Self {
            root,
            selectors: Vec::new(),
            through_borrow: false,
        }
    }

    /// Whether every step this path takes is a field, so each `(owner, index)`
    /// along it is a key a caller can look a write up by. An `Index` or a
    /// `Variant` names no type of its own, and a `Field` past one answers for
    /// the element rather than the container the caller holds.
    #[must_use]
    pub fn field_addressable(&self) -> bool {
        !self
            .selectors
            .iter()
            .any(|s| matches!(s, Selector::Index | Selector::Variant(_)))
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

/// What each local stands for. Covers every binder that hands out a name: a
/// borrow, an accessor's result, a destructuring pattern, a re-seated
/// reference.
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

/// The projection an accessor returns out of its receiver.
#[derive(Clone, Debug)]
pub struct ReturnPath {
    pub selectors: Vec<Selector>,
    /// [`Place::through_borrow`] for the place this path lands on, so even a
    /// fresh receiver does not own it.
    pub through_borrow: bool,
}

/// Which projection of its receiver each accessor returns. A call with no
/// entry returns storage this walk cannot place.
pub type ReturnPaths = FuncKeyMap<ReturnPath>;

/// The projection each function returns out of its first parameter, for the
/// calls that name storage rather than build it.
///
/// A least fixpoint, because an accessor is routinely written over another one
/// (`fn first(&self) -> &T { return self.rows.get(0) }`) and a single pass
/// resolves the inner call to [`Names::Unknown`]. Monotone: knowing more only
/// turns an `Unknown` into a place, so a recorded entry stays valid.
#[must_use]
pub fn compute_return_paths(
    flat: &crate::flat_package::FlatPackage,
    call_graph: &super::callgraph::CallGraph,
    type_table: &TypeTable,
    returns_owned: &FuncKeySet,
) -> ReturnPaths {
    let mut paths = ReturnPaths::default();
    call_graph.solve(flat, |id| {
        let func = flat.functions[id as usize].borrow();
        if paths.get(&func.module_source, &func.name).is_some() {
            return false;
        }
        let (Some(body), Some(receiver)) = (&func.body, func.params.first()) else {
            return false;
        };
        // A result that could name storage: a reference, or a value the copy
        // rules defend — the latter because a returned construction hands its
        // payload out uncopied.
        if !is_reference(func.return_type, type_table)
            && !super::needs_value_copy(func.return_type, type_table)
        {
            return false;
        }
        let resolver = Resolver::new(&func, type_table, &paths, returns_owned);
        let mut returned = ReturnedPlace {
            resolver: &resolver,
            names: None,
        };
        returned.visit_block(body);
        let Some(Names::Place(place)) = returned.names else {
            return false;
        };
        if place.root != receiver.local_index {
            return false;
        }
        paths.insert(
            func.module_source.clone(),
            func.name.clone(),
            ReturnPath {
                selectors: place.selectors,
                through_borrow: place.through_borrow,
            },
        );
        true
    });
    paths
}

/// The single place a body returns, or `None` where it returns more than one.
struct ReturnedPlace<'r, 'a> {
    resolver: &'r Resolver<'a>,
    names: Option<Names>,
}

impl TirRefVisitor for ReturnedPlace<'_, '_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if let TirStmtKind::Return { value: Some(value) } = &stmt.kind {
            // A returned construction hands its payload out uncopied, so the
            // storage the call names is the payload's; an empty case carries
            // none and abstains.
            let value = super::analyze::returned_value(value, true);
            if !super::analyze::carries_no_storage(value) {
                let names = self.resolver.names(value);
                self.names = match self.names.take() {
                    None => Some(names),
                    Some(seen) if seen == names => Some(seen),
                    Some(_) => Some(Names::Unknown),
                };
            }
        }
        self.walk_stmt(stmt);
    }
}

/// Resolve expressions of one function against what its locals stand for.
pub struct Resolver<'a> {
    type_table: &'a TypeTable,
    /// What each accessor returns out of its receiver.
    return_paths: &'a ReturnPaths,
    /// Callees whose result is storage of its own. Every other hands back
    /// something this walk will not guess at.
    returns_owned: &'a FuncKeySet,
    /// Parameters naming storage the caller lent, by the type lent. The only
    /// roots a write in this body reaches out through.
    lent: IndexMap<u32, TypeId>,
    bindings: Bindings,
}

impl<'a> Resolver<'a> {
    /// Walk `func` once, recording what each of its locals stands for. A
    /// reference parameter stands for storage its caller lent.
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
                    .insert(param.local_index, field_owner(param.type_id, type_table));
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

    /// What `local` stands for, where a binder gave it a name.
    #[must_use]
    pub fn binding(&self, local: u32) -> Option<Names> {
        self.bindings.get(local).cloned()
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
            // A reference field borrows storage its holder does not own — an
            // iterator's `repr` borrows the list it walks — so a fresh aggregate
            // does not make what it points at fresh.
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                let names = self.project(
                    inner,
                    Selector::Field {
                        owner: field_owner(inner.type_id, self.type_table),
                        index: *field_index,
                    },
                );
                // `is_reference`, not a `ResolvedType` match: `boxing` has
                // already rewritten some `&T` onto `Box<T>` in place, and which
                // ones it reached says nothing about whether the field borrows.
                if !is_reference(expr.type_id, self.type_table) {
                    return names;
                }
                match names {
                    Names::Place(mut place) => {
                        place.through_borrow = true;
                        Names::Place(place)
                    }
                    Names::Value | Names::Unknown => Names::Unknown,
                }
            }
            TirExprKind::VariantPayload {
                expr: inner,
                case_index,
                ..
            } => self.project(inner, Selector::Variant(*case_index)),
            TirExprKind::Index { expr: inner, .. } => self.project(inner, Selector::Index),
            TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary {
                op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                expr: inner,
            } => self.names(inner),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => match self.names(inner) {
                Names::Place(p) => Names::Place(p),
                Names::Value | Names::Unknown => Names::Unknown,
            },
            // An element read borrows the slot in place, so it names its
            // container's storage one `Index` further in.
            TirExprKind::Call { func, args, .. }
                if func.module_source.is_core_builtin()
                    && super::ownership::is_container_alias_read(
                        &func.name,
                        func.monomorph_info.as_ref(),
                    ) =>
            {
                match args.first() {
                    Some(arg) => self.project(&arg.expr, Selector::Index),
                    None => Names::Unknown,
                }
            }
            TirExprKind::Call { func, args, .. } => {
                match self.return_paths.get(&func.module_source, &func.name) {
                    Some(path) => match args.first().map(|r| self.names(&r.expr)) {
                        Some(Names::Place(mut place)) => {
                            place.selectors.extend(path.selectors.iter().copied());
                            place.through_borrow |= path.through_borrow;
                            Names::Place(place)
                        }
                        // A fresh receiver does not own what it borrows.
                        Some(Names::Value) if path.through_borrow => Names::Unknown,
                        Some(other) => other,
                        None => Names::Unknown,
                    },
                    None if self.returns_owned.contains(&func.module_source, &func.name) => {
                        Names::Value
                    }
                    None => Names::Unknown,
                }
            }
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::ArrayLiteral { .. }
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
            Names::Value => Names::Value,
            Names::Unknown => Names::Unknown,
        }
    }

    fn bind_pattern(&mut self, pattern: &TirPattern, base: &Names) {
        match pattern {
            TirPattern::Binding { local_index, .. } => {
                self.bindings.set(*local_index, base.clone());
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
                            // A destructured field names the same storage
                            // `base.field` does, but the pattern carries no
                            // type to say whether that field borrows. Answer
                            // borrowed: refusing a share costs a copy, allowing
                            // one against a borrow costs the value semantics.
                            place.through_borrow = true;
                            Names::Place(place)
                        }
                        Names::Value => Names::Value,
                        Names::Unknown => Names::Unknown,
                    };
                    self.bind_pattern(&field.pattern, &nested);
                }
            }
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

/// The root local a place expression is taken over. Needs no types, so a
/// consumer wanting only the root needs no resolver.
#[must_use]
pub fn place_root(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } => place_root(inner),
        _ => None,
    }
}

/// The type a [`Selector::Field`] is keyed by. Minted here alone, so a callee
/// recording a write and a caller looking it up cannot spell the key apart.
/// Peels `Box<T>` along with `Ref`/`MutRef`: a callee's own `&mut T`
/// parameter is never boxed, but a caller's address-taken local passed to it
/// is, by the time this walk runs — the two must key the same write alike.
#[must_use]
pub fn field_owner(base_type: TypeId, type_table: &TypeTable) -> TypeId {
    type_table.peel_refs_and_box(base_type)
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
                local_index,
                type_id,
                value,
                ..
            } => {
                // A reference-typed local redirects reads through it to its
                // referent; any other local owns its own storage, so it stays
                // its own place even when the value it starts with came from
                // elsewhere. Keyed on the local's own declared type, not the
                // initializer's — a payload projected out of a generic
                // associated type can still carry the unresolved projection
                // as its own `type_id` where the local's declared type is
                // already the monomorphized reference.
                let names = if is_reference(*type_id, self.resolver.type_table) {
                    self.resolver.names(value)
                } else {
                    Names::Place(Place::local(*local_index))
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
