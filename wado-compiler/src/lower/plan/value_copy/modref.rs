//! What a call writes into its receiver, as fields of the receiver's type. A
//! caller reading one field while a call writes another needs no defensive
//! copy; a callee this analysis cannot read through still writes everything,
//! which is the answer every call used to get.

use super::funcset::{FuncKeyMap, FuncKeySet};
use super::place::{Names, Place, Resolver, ReturnPaths, Selector, could_write_through};
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

    /// The fields of `owner` this writes, for a caller re-rooting them at its
    /// own receiver.
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
#[derive(Default)]
pub struct ModRef {
    per_func: FuncKeyMap<Writes>,
}

impl ModRef {
    /// What calling `(module, name)` writes. A callee with no entry writes
    /// anything.
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
pub fn compute_mod_ref(flat: &FlatPackage, returns_owned: &FuncKeySet) -> ModRef {
    let type_table = flat.type_table.borrow();
    let return_paths = super::place::compute_return_paths(flat, &type_table, returns_owned);
    // A body this scan reads. One without — an import, a builtin — reaches the
    // caller only through what it is handed, which the call site answers for.
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
        let (writes, callees) = scan(&func, &type_table, &defined, &return_paths, returns_owned);
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
                merged.absorb(&callee);
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
    return_paths: &ReturnPaths,
    returns_owned: &FuncKeySet,
) -> (Writes, Vec<(ModuleSource, String)>) {
    if func.body.is_none() {
        // No body to read: whatever it does, it does where nothing can see.
        return (
            Writes {
                opaque: true,
                ..Writes::default()
            },
            Vec::new(),
        );
    }
    let resolver = Resolver::new(func, type_table, return_paths, returns_owned);
    let mut walker = Walker {
        type_table,
        defined,
        resolver: &resolver,
        writes: Writes::default(),
        callees: Vec::new(),
    };
    if let Some(body) = &func.body {
        walker.visit_block(body);
    }
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
    /// Record a write to what `names` stands for.
    ///
    /// A path through fields names them; one that stops at a root names the
    /// whole of what the caller lent, or nothing where the root is a local of
    /// this function's own. A place the resolver could not follow is every
    /// write at once.
    fn record(&mut self, names: &Names) {
        match names {
            Names::Place(place) => {
                let mut named_a_field = false;
                for selector in &place.selectors {
                    if let Selector::Field { owner, index } = selector {
                        self.writes.fields.insert((*owner, *index));
                        named_a_field = true;
                    }
                }
                if !named_a_field && let Some(lent) = self.resolver.lent(place.root) {
                    self.writes.whole.insert(lent);
                }
            }
            Names::Value => {}
            Names::Unknown => self.writes.opaque = true,
        }
    }

    /// Record only the fields a path names, leaving what a callee does inside
    /// its own parameter to the call graph.
    fn record_fields(&mut self, names: &Names) {
        match names {
            Names::Place(place) => {
                for selector in &place.selectors {
                    if let Selector::Field { owner, index } = selector {
                        self.writes.fields.insert((*owner, *index));
                    }
                }
            }
            Names::Value => {}
            Names::Unknown => self.writes.opaque = true,
        }
    }
}

impl TirRefVisitor for Walker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // Re-seating a reference writes no storage; what it now names the
            // resolver already recorded.
            TirExprKind::Assign { target, .. } => {
                let reseats = matches!(target.kind, TirExprKind::Local { .. })
                    && super::place::is_reference(target.type_id, self.type_table);
                if !reseats {
                    let names = self.resolver.names(target);
                    self.record(&names);
                }
            }
            // The callee's own writes arrive through the call graph; what it
            // writes through a handle it is given lands in the place this body
            // lent it.
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
                    if known {
                        self.record_fields(&names);
                    } else {
                        self.record(&handed_out(&names, arg.expr.type_id, self.type_table));
                    }
                }
            }
            // A closure reaches this function's storage only through what it is
            // handed: a capture of a place is a borrow taken where the closure
            // was built, in the body that lent the storage.
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args
                    .iter()
                    .filter(|a| could_write_through(a.type_id, self.type_table))
                {
                    let names = self.resolver.names(arg);
                    self.record(&handed_out(&names, arg.type_id, self.type_table));
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// What a callee with no body reaches through a handle: the fields the place
/// names, or — where it names none — the whole of what it was handed.
fn handed_out(names: &Names, handed: TypeId, type_table: &TypeTable) -> Names {
    let Names::Place(place) = names else {
        return names.clone();
    };
    if place
        .selectors
        .iter()
        .any(|s| matches!(s, Selector::Field { .. }))
    {
        return names.clone();
    }
    Names::Place(Place {
        root: place.root,
        selectors: vec![Selector::Field {
            owner: type_table.peel_refs(handed),
            index: u32::MAX,
        }],
    })
}
