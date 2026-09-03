//! What a call writes through a `&mut` it is handed, as fields of the type
//! carrying them. A callee this analysis cannot read through writes everything.

use super::funcset::{FuncKeyMap, FuncKeySet};
use super::place::{Names, Resolver, ReturnPaths, Selector, could_write_through, field_owner};
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::tir::{TirExpr, TirExprKind, TirFunction, TirStmt, TypeId, TypeTable};
use crate::tir_visitor::TirRefVisitor;

/// The fields one function writes, by the type carrying each.
#[derive(Default, Clone, PartialEq)]
pub struct Writes {
    fields: IndexSet<(TypeId, u32)>,
    /// Types written past any one field: what a `*p = v` through a reference
    /// parameter replaces.
    whole: IndexSet<TypeId>,
    /// A write this analysis could not name. Everything is written.
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

    /// The fields of `owner` this writes, for a caller re-rooting them at the
    /// handle it passed.
    pub fn fields_of(&self, owner: TypeId) -> impl Iterator<Item = u32> + '_ {
        self.fields
            .iter()
            .filter(move |(ty, _)| *ty == owner)
            .map(|(_, field)| *field)
    }

    fn absorb(&mut self, other: &Writes) {
        self.opaque |= other.opaque;
        for f in &other.fields {
            self.fields.insert(*f);
        }
        for t in &other.whole {
            self.whole.insert(*t);
        }
    }
}

/// Every function's writes, closed over the call graph.
pub struct ModRef {
    per_func: FuncKeyMap<Writes>,
    unknown: Writes,
}

impl ModRef {
    /// What calling `(module, name)` writes. A callee with no entry writes
    /// anything.
    #[must_use]
    pub fn writes(&self, module: &ModuleSource, name: &str) -> &Writes {
        self.per_func.get(module, name).unwrap_or(&self.unknown)
    }
}

/// What a path naming no field of its own writes.
#[derive(Clone, Copy)]
enum WholeOf {
    /// Nothing. A callee's writes inside its own parameter arrive through the
    /// call graph, not through the argument.
    Nothing,
    /// What this body was lent at the path's root.
    Lent,
    /// The type handed over, for a handle this walk cannot follow.
    Handed(TypeId),
}

/// Collect each body's own writes, then close over the call graph: a caller
/// writes what its callees write.
#[must_use]
pub fn compute_mod_ref(
    flat: &FlatPackage,
    return_paths: &ReturnPaths,
    returns_owned: &FuncKeySet,
) -> ModRef {
    let type_table = flat.type_table.borrow();
    // A body this scan reads. One without reaches the caller only through what
    // it is handed, which the call site answers for.
    let mut defined = FuncKeySet::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if func.body.is_some() {
            defined.insert(func.module_source.clone(), func.name.clone());
        }
    }

    let mut direct: Vec<(ModuleSource, String, Writes, Vec<(ModuleSource, String)>)> = Vec::new();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        let (writes, callees) = scan(&func, &type_table, &defined, return_paths, returns_owned);
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
                if let Some(callee) = per_func.get(cm, cn) {
                    merged.absorb(callee);
                }
            }
            if per_func.get(module, name) != Some(&merged) {
                per_func.insert(module.clone(), name.clone(), merged);
                changed = true;
            }
        }
    }
    ModRef {
        per_func,
        unknown: Writes {
            opaque: true,
            ..Writes::default()
        },
    }
}

fn scan(
    func: &TirFunction,
    type_table: &TypeTable,
    defined: &FuncKeySet,
    return_paths: &ReturnPaths,
    returns_owned: &FuncKeySet,
) -> (Writes, Vec<(ModuleSource, String)>) {
    let Some(body) = &func.body else {
        return (
            Writes {
                opaque: true,
                ..Writes::default()
            },
            Vec::new(),
        );
    };
    let resolver = Resolver::new(func, type_table, return_paths, returns_owned);
    let mut walker = Walker {
        type_table,
        defined,
        resolver: &resolver,
        writes: Writes::default(),
        callees: Vec::new(),
    };
    walker.visit_block(body);
    (walker.writes, walker.callees)
}

struct Walker<'a> {
    type_table: &'a TypeTable,
    defined: &'a FuncKeySet,
    resolver: &'a Resolver<'a>,
    writes: Writes,
    callees: Vec<(ModuleSource, String)>,
}

impl Walker<'_> {
    /// Record a write to what `names` stands for: every field the path names,
    /// and where it names none, `whole`.
    fn record(&mut self, names: &Names, whole: WholeOf) {
        let Names::Place(place) = names else {
            self.writes.opaque |= matches!(names, Names::Unknown);
            return;
        };
        let mut named_a_field = false;
        for selector in &place.selectors {
            if let Selector::Field { owner, index } = selector {
                self.writes.fields.insert((*owner, *index));
                named_a_field = true;
            }
        }
        if named_a_field {
            return;
        }
        let whole = match whole {
            WholeOf::Nothing => None,
            WholeOf::Lent => self.resolver.lent(place.root),
            WholeOf::Handed(ty) => Some(field_owner(ty, self.type_table)),
        };
        if let Some(whole) = whole {
            self.writes.whole.insert(whole);
        }
    }

    /// Record a writable handle escaping into an aggregate as a write to what
    /// it borrows: the aggregate carries it past the expression that took it.
    fn record_handle_escape(&mut self, value: &TirExpr) {
        if could_write_through(value.type_id, self.type_table) {
            let names = self.resolver.names(value);
            self.record(&names, WholeOf::Handed(value.type_id));
        }
    }
}

impl TirRefVisitor for Walker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // Re-seating a reference writes no storage.
            TirExprKind::Assign { target, .. } => {
                let reseats = matches!(target.kind, TirExprKind::Local { .. })
                    && super::place::is_reference(target.type_id, self.type_table);
                if !reseats {
                    let names = self.resolver.names(target);
                    self.record(&names, WholeOf::Lent);
                }
            }
            // A callee this scan reads answers for its own parameter through the
            // call graph; one it does not writes what it was handed.
            TirExprKind::Call { func, args, .. } => {
                let known = self.defined.contains(&func.module_source, &func.name);
                if known {
                    self.callees
                        .push((func.module_source.clone(), func.name.clone()));
                }
                for arg in args
                    .iter()
                    .filter(|a| could_write_through(a.expr.type_id, self.type_table))
                {
                    let names = self.resolver.names(&arg.expr);
                    let whole = if known {
                        WholeOf::Nothing
                    } else {
                        WholeOf::Handed(arg.expr.type_id)
                    };
                    self.record(&names, whole);
                }
            }
            // A closure reaches a frame only through its captures, which the
            // frame that builds one accounts for.
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args
                    .iter()
                    .filter(|a| could_write_through(a.type_id, self.type_table))
                {
                    let names = self.resolver.names(arg);
                    self.record(&names, WholeOf::Handed(arg.type_id));
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.record_handle_escape(&field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                for element in elements {
                    self.record_handle_escape(element);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(payload),
                ..
            } => self.record_handle_escape(payload),
            _ => {}
        }
        self.walk_expr(expr);
    }
}
