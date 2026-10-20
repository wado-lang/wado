//! The affine move check (WEP: Affine Types): a body reads an affine value at
//! most once, and a named declaration says whether it is affine. It runs on the
//! `Semantics` layer, over every module, and needs no instantiation: a generic
//! body is checked once, with its `affine` parameters affine.

use crate::ast::{
    self, AssignExpr, AstId, Block, ClosureExpr, Condition, ConditionElement, Expr, Function,
    GenericParam, IdentExpr, ImplBlock, Item, Pattern, Stmt, TraitDecl, for_each_pattern_binding,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::compiler_item::CompilerItem;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::semantics::{Operand, Semantics};
use crate::symbol::SymbolKind;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;
use crate::unparse::unparse_type_into;

#[derive(Debug, Clone)]
pub struct ResourceMoveError {
    kind: MoveError,
    span: Span,
    module: String,
}

#[derive(Debug, Clone)]
enum MoveError {
    /// A binding read after a by-value use moved it.
    UseAfterMove { name: String, moved_at: Span },
    /// An affine value read by value through a reference, which leaves it with
    /// its owner: the read would be a second owner.
    MoveOutOfReference { type_name: String },
    /// A closure body moving a binding it captured, by reference.
    CapturedMove { name: String },
    /// A guard moving a binding of its arm, which it sees by reference.
    GuardMove { name: String },
    /// A global holding an affine value, read by value.
    GlobalByValue { name: String },
    /// A named struct or variant holding an affine value without `affine`.
    AffineUndeclared {
        keyword: &'static str,
        name: String,
        /// The field or case that holds it.
        member: String,
    },
    /// A named struct or variant declared `affine` that holds no affine value.
    AffineHoldsNothing { keyword: &'static str, name: String },
    /// An affine type implementing a trait not declared `affine`.
    UnmarkedTraitImpl {
        type_name: String,
        trait_name: String,
    },
    /// A copyable type implementing `Drop`, which every copy would run.
    DropOnCopyable { type_name: String },
    /// A field moved out of a value whose type implements `Drop`.
    DropPartialMove { type_name: String },
    /// `..base` over a type that implements `Drop`.
    DropSpread { type_name: String },
    /// A value read after a local it refers into moved, dropped, or went to a
    /// `&mut` use other than through the value.
    DanglingReference {
        holder: String,
        owner: String,
        gone: Gone,
    },
    /// A body returning a reference into a local of its own.
    ReturnsLocalReference { owner: String },
    /// A `&mut` parameter or a capture keeping a reference into a local of the
    /// body past its end.
    ReferenceEscapes { holder: String, owner: String },
    /// A global keeping a reference into a local.
    GlobalHoldsReference { global: String, owner: String },
}

impl From<ResourceMoveError> for Diagnostic {
    fn from(e: ResourceMoveError) -> Self {
        let (code, message) = match e.kind {
            MoveError::UseAfterMove { name, moved_at } => (
                Code::TypeMismatch,
                format!(
                    "affine value `{name}` used after it was moved (moved at {}:{})",
                    moved_at.line, moved_at.column,
                ),
            ),
            MoveError::MoveOutOfReference { type_name } => (
                Code::TypeMismatch,
                format!("cannot move affine type `{type_name}` out of a reference"),
            ),
            MoveError::CapturedMove { name } => (
                Code::TypeMismatch,
                format!("a closure captures `{name}` by reference, so its body cannot move it"),
            ),
            MoveError::GuardMove { name } => (
                Code::TypeMismatch,
                format!("a guard sees `{name}` by reference, so it cannot move it"),
            ),
            MoveError::GlobalByValue { name } => (
                Code::TypeMismatch,
                format!("cannot read global `{name}` by value: it holds an affine value"),
            ),
            MoveError::AffineUndeclared {
                keyword,
                name,
                member,
            } => (
                Code::AffineDeclaration,
                format!(
                    "`{name}` holds an affine value in `{member}`, so it must be declared `affine {keyword}`"
                ),
            ),
            MoveError::AffineHoldsNothing { keyword, name } => (
                Code::AffineDeclaration,
                format!(
                    "`{name}` holds no affine value, so it cannot be declared `affine {keyword}`"
                ),
            ),
            MoveError::UnmarkedTraitImpl {
                type_name,
                trait_name,
            } => (
                Code::AffineDeclaration,
                format!(
                    "`{type_name}` is affine, so it implements only an `affine trait`, and `{trait_name}` is not one"
                ),
            ),
            MoveError::DropOnCopyable { type_name } => (
                Code::AffineDeclaration,
                format!(
                    "`{type_name}` is copyable, so it cannot implement `Drop`: every copy would run it"
                ),
            ),
            MoveError::DropPartialMove { type_name } => (
                Code::TypeMismatch,
                format!(
                    "`{type_name}` implements `Drop`, so no field moves out of it: its `drop` relies on the fields it was built with"
                ),
            ),
            MoveError::DropSpread { type_name } => (
                Code::TypeMismatch,
                format!(
                    "`{type_name}` implements `Drop`, so `..base` cannot take fields from it: its `drop` relies on the fields it was built with"
                ),
            ),
            MoveError::DanglingReference {
                holder,
                owner,
                gone,
            } => (
                Code::TypeMismatch,
                match gone {
                    Gone::Moved(at) => format!(
                        "`{holder}` refers into `{owner}`, which moved at {}:{}",
                        at.line, at.column,
                    ),
                    Gone::Ended => format!(
                        "`{holder}` refers into `{owner}`, which dropped as its scope ended"
                    ),
                    Gone::Disturbed(at) => format!(
                        "`{holder}` refers into `{owner}`, which a `&mut` use at {}:{} may have moved out of",
                        at.line, at.column,
                    ),
                },
            ),
            MoveError::ReturnsLocalReference { owner } => (
                Code::TypeMismatch,
                format!(
                    "cannot return a reference into `{owner}`, which drops as the body returns"
                ),
            ),
            MoveError::ReferenceEscapes { holder, owner } => (
                Code::TypeMismatch,
                format!(
                    "`{holder}` keeps a reference into `{owner}` past the body, where `{owner}` drops"
                ),
            ),
            MoveError::GlobalHoldsReference { global, owner } => (
                Code::TypeMismatch,
                format!("global `{global}` cannot keep a reference into `{owner}`, which drops"),
            ),
        };
        Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&e.span, Some(&e.module))),
        }
    }
}

#[must_use]
pub fn check_resource_moves_semantic(sem: &Semantics) -> Vec<ResourceMoveError> {
    let drops = Drops::of(sem);
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        let mut checker = Checker {
            sem,
            module: src,
            drops: &drops,
            out: &mut out,
        };
        for item in &module.items {
            checker.check_item(item);
        }
    }
    out
}

/// The `Drop` trait and the declarations of the types implementing it.
struct Drops {
    /// `None` in `Semantics::empty`, which the language service checks after
    /// a failed load: it declares nothing, so nothing implements `Drop`.
    trait_id: Option<AstId>,
    implementors: IndexSet<AstId>,
}

impl Drops {
    fn of(sem: &Semantics) -> Drops {
        let mut drops = Drops {
            trait_id: sem
                .types
                .compiler_item_def(CompilerItem::Drop)
                .map(|def| sem.types.defs().ast_id(def)),
            implementors: IndexSet::default(),
        };
        drops.implementors = sem
            .modules
            .values()
            .flat_map(|module| &module.items)
            .filter_map(|item| match item {
                Item::Impl(b) if drops.implemented_by(b, sem) => {
                    sem.declaration_of(&b.ty).map(Item::id)
                }
                _ => None,
            })
            .collect();
        drops
    }

    fn implemented_by(&self, b: &ImplBlock, sem: &Semantics) -> bool {
        self.trait_id
            .is_some_and(|id| sem.implemented(b).map(Item::id) == Some(id))
    }
}

/// The names of `params` declared `affine`.
fn affine_params<'a>(params: impl IntoIterator<Item = &'a GenericParam>) -> Vec<String> {
    params
        .into_iter()
        .filter(|p| p.is_affine)
        .map(|p| p.name.clone())
        .collect()
}

struct Checker<'a> {
    sem: &'a Semantics,
    module: &'a ModuleSource,
    drops: &'a Drops,
    out: &'a mut Vec<ResourceMoveError>,
}

impl Checker<'_> {
    fn push(&mut self, kind: MoveError, span: Span) {
        self.out.push(ResourceMoveError {
            kind,
            span,
            module: self.module.source_path(),
        });
    }

    fn check_item(&mut self, item: &Item) {
        self.check_affine_declaration(item);
        match item {
            Item::Function(func) => self.check_function(func, &[], false),
            Item::Impl(b) => {
                let scope = affine_params(&b.type_params);
                self.check_impl_trait(b, &scope);
                let drop_body = self.drops.implemented_by(b, self.sem);
                for method in &b.methods {
                    self.check_function(method, &scope, drop_body);
                }
            }
            Item::Trait(t) => {
                let scope = trait_scope(t);
                for method in &t.methods {
                    self.check_function(method, &scope, false);
                }
            }
            Item::Interface(i) => {
                for method in &i.methods {
                    self.check_function(method, &[], false);
                }
            }
            Item::Test(test) => self.check_body(&test.body, Vec::new(), false, Vec::new()),
            _ => {}
        }
    }

    fn check_function(&mut self, func: &Function, outer: &[String], drop_body: bool) {
        let Some(body) = &func.body else {
            return;
        };
        let mut scope = outer.to_vec();
        scope.extend(affine_params(&func.type_params));
        let outside = func
            .params
            .iter()
            .filter(|p| self.is_mut_ref_local(p.id))
            .map(|p| (p.id, p.name.clone()))
            .collect();
        self.check_body(body, scope, drop_body, outside);
    }

    fn is_mut_ref_local(&self, id: AstId) -> bool {
        self.sem
            .local_type(id)
            .is_some_and(|ty| matches!(self.sem.types.get(ty), ResolvedType::MutRef(_)))
    }

    /// `outside` holds the `&mut` parameters, which outlive the body.
    fn check_body(
        &mut self,
        body: &Block,
        affine_params: Vec<String>,
        drop_body: bool,
        outside: Vec<(AstId, String)>,
    ) {
        let mut walker = MoveWalker::new(self, affine_params, drop_body);
        walker.visit_block(body);
        walker.refuse_escapes(&outside);
    }

    /// A named struct or variant is declared `affine` exactly when a field or a
    /// case holds an affine value in every instantiation.
    fn check_affine_declaration(&mut self, item: &Item) {
        let (keyword, id, name, name_span, is_affine, members): (_, _, _, _, _, Vec<&str>) =
            match item {
                Item::Struct(s) => (
                    "struct",
                    s.id,
                    &s.name,
                    s.name_span,
                    s.is_affine,
                    s.fields.iter().map(|f| f.name.as_str()).collect(),
                ),
                Item::Variant(v) => (
                    "variant",
                    v.id,
                    &v.name,
                    v.name_span,
                    v.is_affine,
                    v.cases.iter().map(|c| c.name.as_str()).collect(),
                ),
                _ => return,
            };
        match (self.sem.first_affine_member(id), is_affine) {
            (Some(index), false) => self.push(
                MoveError::AffineUndeclared {
                    keyword,
                    name: name.clone(),
                    member: members[index].to_string(),
                },
                name_span,
            ),
            (None, true) if !self.drops.implementors.contains(&id) => self.push(
                MoveError::AffineHoldsNothing {
                    keyword,
                    name: name.clone(),
                },
                name_span,
            ),
            _ => {}
        }
    }

    /// An affine type implements only an `affine trait`: an unmarked trait's
    /// default bodies are checked with `Self` copyable.
    fn check_impl_trait(&mut self, b: &ImplBlock, scope: &[String]) {
        let Some(trait_type) = &b.trait_type else {
            return;
        };
        let Some(Item::Trait(t)) = self.sem.implemented(b) else {
            return;
        };
        let mut type_name = String::new();
        unparse_type_into(&b.ty, &mut type_name);
        if Some(t.id) == self.drops.trait_id {
            if !self.impl_self_is_affine(b, scope) {
                self.push(MoveError::DropOnCopyable { type_name }, trait_type.span());
            }
            return;
        }
        if t.is_affine || !self.impl_self_is_affine(b, scope) {
            return;
        }
        self.push(
            MoveError::UnmarkedTraitImpl {
                type_name,
                trait_name: t.name.clone(),
            },
            trait_type.span(),
        );
    }

    /// Whether the implementing type is affine: affine in every instantiation,
    /// or written with an `affine` parameter of the impl as an argument.
    fn impl_self_is_affine(&self, b: &ImplBlock, scope: &[String]) -> bool {
        let mut mentioned = Vec::new();
        b.ty.mentioned_names(&mut mentioned);
        let names_affine_param = mentioned.iter().any(|name| scope.contains(name));
        names_affine_param
            || self
                .sem
                .declaration_of(&b.ty)
                .and_then(|decl| self.sem.types.type_of_symbol(&decl.id()))
                .is_some_and(|ty| self.sem.is_affine(ty, &[]))
    }
}

/// An `affine trait`'s default bodies are checked with `Self` affine.
fn trait_scope(t: &TraitDecl) -> Vec<String> {
    let mut scope = affine_params(&t.type_params);
    if t.is_affine {
        scope.push("Self".to_string());
    }
    scope
}

/// What a loop evaluates at the start of each iteration.
enum LoopHead<'a> {
    None,
    /// `while` and C-style `for`: a `let` in it binds afresh each time.
    Condition(&'a Condition),
    /// `for ... of`: the binding takes the next element, which refers into
    /// what the operand refers into.
    Binding(&'a Pattern, Owners),
}

/// Each moved place, a binding and a field path into it, with where it moved.
type Moves = IndexMap<(AstId, Vec<String>), Span>;

/// Why a local a value refers into no longer holds what the value reads.
#[derive(Debug, Clone, Copy)]
enum Gone {
    Moved(Span),
    Ended,
    /// A `&mut` use of the local, not through the value, which may have moved
    /// a part of it out.
    Disturbed(Span),
}

/// A local a reference reaches into: the binding, the field path into it,
/// which binding of it, since a loop binds its locals afresh each iteration,
/// and how many `&mut` uses of it came before.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Owner {
    def: AstId,
    path: Vec<String>,
    instance: u32,
    disturbed: usize,
}

#[derive(Clone)]
struct OwnerSite {
    name: String,
    /// Where the reference was taken.
    span: Span,
    /// Only an affine local moves, and only dropping one ends what a reference
    /// into it reads.
    affine: bool,
    /// Whether the value may write through to the local: a `&mut`, or a
    /// closure making a `&mut` use of its capture.
    writes: bool,
}

/// A `&mut` use of a place: `via` is the binding it went through.
#[derive(Clone)]
struct Disturbance {
    via: AstId,
    path: Vec<String>,
    span: Span,
}

/// What a value refers into.
type Owners = IndexMap<Owner, OwnerSite>;

fn merge_owners(into: &mut Owners, from: Owners) {
    for (owner, site) in from {
        into.entry(owner).or_insert(site);
    }
}

/// What a body knows at a point of it.
#[derive(Clone, Default)]
struct Flow {
    moved: Moves,
    /// What each local holding a reference refers into.
    holders: IndexMap<AstId, Owners>,
}

impl Flow {
    /// The state where paths join: a place moved on any of them, and a
    /// reference held on any of them.
    fn merge(&mut self, from: Flow) {
        for (place, span) in from.moved {
            self.moved.entry(place).or_insert(span);
        }
        for (holder, owners) in from.holders {
            merge_owners(self.holders.entry(holder).or_default(), owners);
        }
    }
}

/// A place a body reads: a binding and the field path into it (`h.f.g`).
struct PlacePath<'e> {
    root: &'e IdentExpr,
    fields: Vec<&'e str>,
}

impl PlacePath<'_> {
    fn owned_fields(&self) -> Vec<String> {
        self.fields.iter().map(|f| (*f).to_string()).collect()
    }
}

struct MoveWalker<'a, 'c> {
    checker: &'c mut Checker<'a>,
    affine_params: Vec<String>,
    /// Whether this is a `Drop::drop` body, which may move fields out of `self`.
    drop_body: bool,
    flow: Flow,
    /// The bindings of the arm whose guard is being walked.
    guard_bindings: IndexSet<AstId>,
    /// Inside a closure body, the bindings it declares itself; every other
    /// local it names is a capture.
    closure_locals: Option<IndexSet<AstId>>,
    /// Inside a closure body, each local it captures, by name.
    captures: IndexMap<AstId, String>,
    /// How many times each binding was bound.
    instances: IndexMap<AstId, u32>,
    /// The instance of each local at the end of its scope.
    ended: IndexMap<AstId, u32>,
    /// The bindings each open scope declares, innermost last.
    scopes: Vec<Vec<AstId>>,
    /// The `&mut` uses of each local, in order.
    disturbances: IndexMap<AstId, Vec<Disturbance>>,
    /// Inside a closure body, the captures it makes a `&mut` use of.
    disturbed_captures: IndexSet<AstId>,
    /// What each closure the body built refers into.
    closure_owners: IndexMap<AstId, Owners>,
    /// What the `break` values of each labeled block refer into.
    break_owners: IndexMap<String, Owners>,
    suppress: u32,
}

impl<'a, 'c> MoveWalker<'a, 'c> {
    fn new(checker: &'c mut Checker<'a>, affine_params: Vec<String>, drop_body: bool) -> Self {
        MoveWalker {
            checker,
            affine_params,
            drop_body,
            flow: Flow::default(),
            guard_bindings: IndexSet::default(),
            closure_locals: None,
            captures: IndexMap::default(),
            instances: IndexMap::default(),
            ended: IndexMap::default(),
            scopes: Vec::new(),
            disturbances: IndexMap::default(),
            disturbed_captures: IndexSet::default(),
            closure_owners: IndexMap::default(),
            break_owners: IndexMap::default(),
            suppress: 0,
        }
    }
}

impl MoveWalker<'_, '_> {
    fn sem(&self) -> &Semantics {
        self.checker.sem
    }

    fn is_affine(&self, type_id: TypeId) -> bool {
        self.sem().is_affine(type_id, &self.affine_params)
    }

    fn expr_is_affine(&self, id: AstId) -> bool {
        self.sem()
            .expression_type(id)
            .is_some_and(|ty| self.is_affine(ty))
    }

    fn report(&mut self, kind: MoveError, span: Span) {
        if self.suppress == 0 {
            self.checker.push(kind, span);
        }
    }

    /// The moved place overlapping `def.path`, if any: the place itself, one
    /// inside it, or one holding it.
    fn moved_overlap(&self, def: AstId, path: &[&str]) -> Option<Span> {
        self.flow
            .moved
            .iter()
            .find_map(|((moved_def, moved_path), span)| {
                let overlaps = *moved_def == def && paths_overlap(moved_path, path);
                overlaps.then_some(*span)
            })
    }

    fn place_path(expr: &Expr) -> Option<PlacePath<'_>> {
        match expr {
            Expr::Ident(root) => Some(PlacePath {
                root,
                fields: Vec::new(),
            }),
            Expr::FieldAccess(fa) => {
                let mut path = Self::place_path(&fa.expr)?;
                path.fields.push(&fa.field);
                Some(path)
            }
            _ => None,
        }
    }

    /// Whether `expr` is a container's element, or a field of one at any
    /// depth. A tuple's `t[0]` is its field `t.0`, no element.
    fn roots_in_element(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Index(ix) => !self.sem().expression_type(ix.expr.id()).is_some_and(|ty| {
                let types = &self.sem().types;
                matches!(types.get(types.peel_refs(ty)), ResolvedType::GenericInstance { def, .. }
                    if types.is_tuple_def(*def))
            }),
            Expr::FieldAccess(fa) => self.roots_in_element(&fa.expr),
            _ => false,
        }
    }

    /// Whether `expr` is a reference, so a field read through it reads the
    /// referent.
    fn is_reference(&self, expr: &Expr) -> bool {
        self.sem().expression_type(expr.id()).is_some_and(|ty| {
            matches!(
                self.sem().types.get(ty),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        })
    }

    /// Whether a field path passes through a reference or a dereference.
    fn path_through_reference(&self, expr: &Expr) -> bool {
        match expr {
            Expr::FieldAccess(fa) => {
                matches!(&fa.expr, Expr::Unary(u) if u.op == ast::UnaryOp::Deref)
                    || self.is_reference(&fa.expr)
                    || self.path_through_reference(&fa.expr)
            }
            _ => false,
        }
    }

    fn read(&mut self, place: &PlacePath<'_>) {
        let Some(def) = self.local_use(place.root) else {
            return;
        };
        if let Some(moved_at) = self.moved_overlap(def, &place.fields) {
            self.report(
                MoveError::UseAfterMove {
                    name: place.root.name.clone(),
                    moved_at,
                },
                place.root.span,
            );
        }
    }

    /// The binding `ident` reads, once it is noted as a capture where it is one
    /// and what it holds is checked to be live.
    fn local_use(&mut self, ident: &IdentExpr) -> Option<AstId> {
        let def = self.note_capture(ident)?;
        self.refuse_dangling(def, ident);
        Some(def)
    }

    /// The binding `ident` names, noted as a capture where it is one.
    fn note_capture(&mut self, ident: &IdentExpr) -> Option<AstId> {
        let def = self.sem().referenced_symbol(ident.id)?;
        if self.sem().local_type(def).is_some()
            && let Some(locals) = &self.closure_locals
            && !locals.contains(&def)
        {
            self.captures.insert(def, ident.name.clone());
        }
        Some(def)
    }

    /// Refuse a read of `holder` once a local it refers into moved or dropped.
    fn refuse_dangling(&mut self, holder: AstId, ident: &IdentExpr) {
        let Some(owners) = self.flow.holders.get(&holder) else {
            return;
        };
        let gone = owners.iter().find_map(|(owner, site)| {
            site.affine
                .then(|| self.owner_gone(holder, owner))
                .flatten()
                .map(|gone| (site.name.clone(), gone))
        });
        if let Some((owner, gone)) = gone {
            self.report(
                MoveError::DanglingReference {
                    holder: ident.name.clone(),
                    owner,
                    gone,
                },
                ident.span,
            );
        }
    }

    /// Why `owner` no longer holds what `holder` reads through it, if it does
    /// not.
    fn owner_gone(&self, holder: AstId, owner: &Owner) -> Option<Gone> {
        if self.instance(owner.def) != owner.instance
            || self.ended.get(&owner.def) == Some(&owner.instance)
        {
            return Some(Gone::Ended);
        }
        let path: Vec<&str> = owner.path.iter().map(String::as_str).collect();
        if let Some(moved_at) = self.moved_overlap(owner.def, &path) {
            return Some(Gone::Moved(moved_at));
        }
        let since = self.disturbances.get(&owner.def)?.get(owner.disturbed..)?;
        since
            .iter()
            .find(|d| d.via != holder && paths_overlap(&d.path, &path))
            .map(|d| Gone::Disturbed(d.span))
    }

    fn disturbed_count(&self, def: AstId) -> usize {
        self.disturbances.get(&def).map_or(0, Vec::len)
    }

    /// A `&mut` use of the place `expr` writes through: of the local it is
    /// rooted in, and of each affine local that local writes through to.
    fn disturb(&mut self, expr: &Expr) {
        let Some(root) = write_root(expr) else {
            return;
        };
        let Some(via) = self.sem().referenced_symbol(root.id) else {
            return;
        };
        let path: Vec<String> = Self::place_path(expr)
            .map(|place| place.owned_fields())
            .unwrap_or_default();
        let mut places = vec![(via, path)];
        if let Some(owners) = self.flow.holders.get(&via) {
            places.extend(
                owners
                    .iter()
                    .filter(|(_, site)| site.writes)
                    .map(|(owner, _)| (owner.def, owner.path.clone())),
            );
        }
        for (def, path) in places {
            if !self.is_affine_local(def) {
                continue;
            }
            if !self.is_own_local(def) {
                self.disturbed_captures.insert(def);
            }
            self.disturbances.entry(def).or_default().push(Disturbance {
                via,
                path,
                span: expr.span(),
            });
        }
    }

    fn instance(&self, def: AstId) -> u32 {
        self.instances.get(&def).copied().unwrap_or(0)
    }

    /// The place `path` into `def` as it stands now.
    fn owner(&self, def: AstId, path: Vec<String>) -> Owner {
        Owner {
            def,
            path,
            instance: self.instance(def),
            disturbed: self.disturbed_count(def),
        }
    }

    /// A by-value use of the place `expr` names, whose value is affine.
    fn consume(&mut self, expr: &Expr, place: &PlacePath<'_>) {
        let Some(def) = self.local_use(place.root) else {
            return;
        };
        let name = place.root.name.clone();
        if self.global_name(def).is_some() {
            self.report(MoveError::GlobalByValue { name }, place.root.span);
            return;
        }
        if self.path_through_reference(expr) {
            let type_name = self.type_name_of(expr);
            self.report(MoveError::MoveOutOfReference { type_name }, expr.span());
            return;
        }
        if self.guard_bindings.contains(&def) {
            self.report(MoveError::GuardMove { name }, place.root.span);
            return;
        }
        if self
            .closure_locals
            .as_ref()
            .is_some_and(|locals| !locals.contains(&def))
        {
            self.report(MoveError::CapturedMove { name }, place.root.span);
            return;
        }
        if let Some(moved_at) = self.moved_overlap(def, &place.fields) {
            self.report(MoveError::UseAfterMove { name, moved_at }, place.root.span);
            return;
        }
        self.refuse_move_out_of_drop(expr);
        self.flow
            .moved
            .insert((def, place.owned_fields()), place.root.span);
    }

    fn type_name_of(&self, expr: &Expr) -> String {
        let ty = self
            .sem()
            .expression_type(expr.id())
            .expect("an affine read has a type");
        self.sem().types.type_name(ty)
    }

    /// Refuse a move of the field `expr` names out of a holder whose type
    /// implements `Drop`, at any depth. The `self` of a `Drop::drop` body is the
    /// one holder a field may leave: what `drop` leaves behind drops field by
    /// field.
    fn refuse_move_out_of_drop(&mut self, mut expr: &Expr) {
        while let Expr::FieldAccess(fa) = expr {
            let holder = &fa.expr;
            let own_self = self.drop_body && matches!(holder, Expr::Ident(id) if id.name == "self");
            if !own_self && self.implements_drop(holder) {
                let type_name = self.type_name_of(holder);
                self.report(MoveError::DropPartialMove { type_name }, expr.span());
            }
            expr = holder;
        }
    }

    fn implements_drop(&self, expr: &Expr) -> bool {
        let types = &self.sem().types;
        self.sem()
            .expression_type(expr.id())
            .and_then(|ty| types.nominal_def(ty))
            .is_some_and(|def| {
                self.checker
                    .drops
                    .implementors
                    .contains(&types.defs().ast_id(def))
            })
    }

    /// Forgets the moves an assignment to `place` refills: the place and every
    /// place inside it.
    fn clear_place(&mut self, place: &PlacePath<'_>) {
        let Some(def) = self.sem().referenced_symbol(place.root.id) else {
            return;
        };
        self.flow.moved.retain(|(moved_def, moved_path), _| {
            *moved_def != def
                || moved_path.len() < place.fields.len()
                || !paths_overlap(moved_path, &place.fields)
        });
    }

    fn visit_value(&mut self, expr: &Expr) {
        if let Some(place) = Self::place_path(expr) {
            if self.expr_is_affine(expr.id()) {
                self.consume(expr, &place);
            } else {
                self.read(&place);
            }
            return;
        }
        // An element is read through the container's `&self`, which keeps it.
        let out_of_reference = match expr {
            Expr::Unary(u) if u.op == ast::UnaryOp::Deref => Some(&u.expr),
            Expr::FieldAccess(fa)
                if self.path_through_reference(expr) || self.roots_in_element(&fa.expr) =>
            {
                Some(&fa.expr)
            }
            _ if self.roots_in_element(expr) => Some(expr),
            _ => None,
        };
        match out_of_reference {
            Some(base) if self.expr_is_affine(expr.id()) => {
                self.visit_expr(base);
                let type_name = self.type_name_of(expr);
                self.report(MoveError::MoveOutOfReference { type_name }, expr.span());
            }
            _ => self.visit_expr(expr),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Some(place) = Self::place_path(expr) {
            self.read(&place);
            return;
        }
        match expr {
            Expr::Ident(_) => unreachable!("a place path"),
            // A field of a temporary, which owns it.
            Expr::FieldAccess(fa) => self.visit_expr(&fa.expr),

            Expr::Call(call) => {
                self.visit_expr(&call.callee);
                for arg in &call.args {
                    self.visit_value(arg);
                }
                self.store_through_call(None, &call.args);
                // A closure writes through to what it captures.
                if matches!(call.callee, Expr::Ident(_)) {
                    self.disturb(&call.callee);
                }
            }
            Expr::MethodCall(mc) => {
                if self.sem().method_call_consumes_receiver(mc.id) {
                    self.visit_value(&mc.receiver);
                } else {
                    self.visit_expr(&mc.receiver);
                }
                for arg in &mc.args {
                    self.visit_value(arg);
                }
                self.store_through_call(Some(mc), &mc.args);
            }
            Expr::StaticMethodCall(smc) => {
                for arg in &smc.args {
                    self.visit_value(arg);
                }
                self.store_through_call(None, &smc.args);
            }

            Expr::Unary(u) => self.visit_expr(&u.expr),
            Expr::Binary(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
            }
            Expr::Assign(a) => self.visit_assign(a),
            Expr::CompoundAssign(a) => {
                self.visit_expr(&a.target);
                self.visit_expr(&a.value);
            }
            Expr::Index(ix) => {
                self.visit_expr(&ix.expr);
                self.visit_expr(&ix.index);
            }
            Expr::Cast(c) => self.visit_expr(&c.expr),
            Expr::TryOp(t) => self.visit_value(&t.expr),

            Expr::StructLiteral(s) => {
                if let Some(spread) = s.spreads.first()
                    && self.implements_drop(expr)
                {
                    let type_name = self.type_name_of(expr);
                    self.report(MoveError::DropSpread { type_name }, spread.expr.span());
                }
                for spread in &s.spreads {
                    self.visit_value(&spread.expr);
                }
                for field in &s.fields {
                    self.visit_value(&field.value);
                }
            }
            Expr::TupleLiteral(t) => {
                for el in &t.elements {
                    self.visit_value(el);
                }
            }
            Expr::TupleComprehension(c) => {
                self.visit_value(&c.iterable);
                self.visit_value(&c.body);
            }
            Expr::TemplateString(ts) => {
                for expr in ts.interpolations() {
                    self.visit_expr(expr);
                }
            }
            // A hole is read or referenced where it stands, never moved.
            Expr::TaggedTemplate(t) => {
                for expr in t.template.interpolations() {
                    self.visit_expr(expr);
                }
            }

            Expr::Block(block) => {
                self.visit_block(block);
            }
            Expr::LabeledBlock(lb) => {
                self.visit_block(&lb.block);
            }
            Expr::If(if_expr) => {
                self.visit_if(
                    &if_expr.condition,
                    &if_expr.then_block,
                    if_expr.else_block.as_ref(),
                );
            }
            Expr::Match(m) => {
                self.visit_match(&m.expr, &m.arms);
            }

            Expr::WithHandler(w) => {
                for handler in &w.handlers {
                    self.visit_expr(&handler.handler);
                }
                self.visit_block(&w.body);
            }
            Expr::Range(r) => {
                self.visit_expr(&r.start);
                self.visit_expr(&r.end);
            }
            Expr::Matches(m) => self.visit_expr(&m.expr),
            Expr::Spread(inner, _) => self.visit_expr(inner),
            Expr::ComparisonChain(c) => {
                self.visit_expr(&c.first);
                for cmp in &c.comparisons {
                    self.visit_expr(&cmp.right);
                }
            }
            Expr::Closure(closure) => self.visit_closure(closure),

            Expr::Literal(_) | Expr::Error(_) => {}
        }
    }

    /// A closure body is a body of its own, which captures by reference: it
    /// sees what the frame moved before it, and moves only its own locals.
    fn visit_closure(&mut self, closure: &ClosureExpr) {
        let locals: IndexSet<AstId> = closure.params.iter().map(|p| p.id).collect();
        let outside_params: Vec<(AstId, String)> = closure
            .params
            .iter()
            .filter(|p| self.checker.is_mut_ref_local(p.id))
            .map(|p| (p.id, p.name.clone()))
            .collect();
        let mut inner = MoveWalker::new(
            &mut *self.checker,
            self.affine_params.clone(),
            self.drop_body,
        );
        inner.flow = self.flow.clone();
        inner.guard_bindings = self.guard_bindings.clone();
        inner.closure_locals = Some(locals);
        inner.instances = self.instances.clone();
        inner.ended = self.ended.clone();
        inner.disturbances = self.disturbances.clone();
        inner.suppress = self.suppress;
        inner.visit_value(&closure.body);
        if !matches!(closure.body, Expr::Block(_)) {
            inner.refuse_returned(&closure.body);
        }
        let mut outside = outside_params;
        outside.extend(
            inner
                .captures
                .iter()
                .map(|(def, name)| (*def, name.clone())),
        );
        inner.refuse_escapes(&outside);

        let MoveWalker {
            flow: Flow { mut holders, .. },
            captures,
            closure_locals,
            disturbed_captures,
            ..
        } = inner;
        let locals = closure_locals.expect("a closure body has locals");
        let mut owners = Owners::default();
        for (def, name) in captures {
            // What the body stored in the capture outlives the call.
            let stored: Owners = holders
                .shift_remove(&def)
                .unwrap_or_default()
                .into_iter()
                .filter(|(owner, _)| !locals.contains(&owner.def))
                .collect();
            if !stored.is_empty() {
                merge_owners(self.flow.holders.entry(def).or_default(), stored);
            }
            if let Some(held) = self.flow.holders.get(&def) {
                merge_owners(&mut owners, held.clone());
            }
            let writes = disturbed_captures.contains(&def);
            let site = OwnerSite {
                affine: self.is_affine_local(def),
                name: name.clone(),
                span: closure.span,
                writes,
            };
            owners.insert(self.owner(def, Vec::new()), site);
            if !self.is_own_local(def) {
                self.captures.insert(def, name);
                if writes {
                    self.disturbed_captures.insert(def);
                }
            }
        }
        self.closure_owners.insert(closure.id, owners);
    }

    fn is_affine_local(&self, def: AstId) -> bool {
        self.sem()
            .local_type(def)
            .is_some_and(|ty| self.is_affine(ty))
    }

    fn visit_assign(&mut self, a: &AssignExpr) {
        self.visit_value(&a.value);
        let owners = self.owners_of(&a.value);
        match &a.target {
            Expr::Ident(ident) => {
                if let Some(def) = self.note_capture(ident) {
                    self.rebind(def, owners);
                }
            }
            target => {
                for def in self.write_targets(target) {
                    self.store(def, &owners);
                }
            }
        }
        match Self::place_path(&a.target) {
            Some(place) => self.clear_place(&place),
            None => self.visit_expr(&a.target),
        }
    }

    /// A whole binding takes a new value, referring into `owners` alone.
    fn rebind(&mut self, def: AstId, owners: Owners) {
        if self.refuse_global_store(def, &owners) {
            return;
        }
        if owners.is_empty() {
            self.flow.holders.shift_remove(&def);
        } else {
            self.flow.holders.insert(def, owners);
        }
    }

    /// The binding `def` may now refer into `owners` too, other than itself.
    fn store(&mut self, def: AstId, owners: &Owners) {
        if owners.is_empty() || self.refuse_global_store(def, owners) {
            return;
        }
        let into = self.flow.holders.entry(def).or_default();
        for (owner, site) in owners {
            if owner.def != def {
                into.entry(owner.clone()).or_insert_with(|| site.clone());
            }
        }
    }

    /// Refuse a store of a reference into an affine local in a global, which
    /// outlives every local. True where `def` is a global.
    fn refuse_global_store(&mut self, def: AstId, owners: &Owners) -> bool {
        let Some(global) = self.global_name(def) else {
            return false;
        };
        if let Some(site) = owners.values().find(|site| site.affine) {
            let span = site.span;
            self.report(
                MoveError::GlobalHoldsReference {
                    global,
                    owner: site.name.clone(),
                },
                span,
            );
        }
        true
    }

    /// The name of `def`, where it is a global.
    fn global_name(&self, def: AstId) -> Option<String> {
        self.sem()
            .symbol_at(def)
            .filter(|s| matches!(s.kind, SymbolKind::Global(_)))
            .map(|s| s.name.clone())
    }

    /// The locals a write through `target` may land in: its root, and what the
    /// root refers into.
    fn write_targets(&self, target: &Expr) -> Vec<AstId> {
        let mut targets = Vec::new();
        let Some(root) = write_root(target) else {
            return targets;
        };
        let Some(def) = self.sem().referenced_symbol(root.id) else {
            return targets;
        };
        targets.push(def);
        let mut at = 0;
        while at < targets.len() {
            if let Some(owners) = self.flow.holders.get(&targets[at]) {
                for owner in owners.keys() {
                    if !targets.contains(&owner.def) {
                        targets.push(owner.def);
                    }
                }
            }
            at += 1;
        }
        targets
    }

    /// A call may store what any operand refers into through each `&mut` it
    /// takes.
    fn store_through_call(&mut self, method: Option<&ast::MethodCallExpr>, args: &[Expr]) {
        let mut owners = Owners::default();
        let mut targets = Vec::new();
        if let Some(mc) = method {
            let writes = self.sem().method_call_takes_mut_receiver(mc.id);
            if self.sem().method_call_consumes_receiver(mc.id) {
                merge_owners(&mut owners, self.owners_of(&mc.receiver));
            } else {
                merge_owners(&mut owners, self.reference_owners(&mc.receiver, writes));
            }
            if writes {
                targets.extend(self.write_targets(&mc.receiver));
                self.disturb(&mc.receiver);
            }
        }
        for arg in args {
            merge_owners(&mut owners, self.owners_of(arg));
            if self.is_mut_ref(arg) {
                targets.extend(self.write_targets(arg));
                self.disturb(arg);
            }
        }
        for def in targets {
            if self
                .sem()
                .local_type(def)
                .is_none_or(|ty| self.sem().may_hold_reference(ty))
            {
                self.store(def, &owners);
            }
        }
    }

    fn is_mut_ref(&self, expr: &Expr) -> bool {
        self.sem()
            .expression_type(expr.id())
            .is_some_and(|ty| matches!(self.sem().types.get(ty), ResolvedType::MutRef(_)))
    }

    /// What the value of `expr` refers into.
    fn owners_of(&self, expr: &Expr) -> Owners {
        let may_hold = self
            .sem()
            .expression_type(expr.id())
            .is_some_and(|ty| self.sem().may_hold_reference(ty));
        if !may_hold {
            return Owners::default();
        }
        let mut owners = Owners::default();
        match expr {
            Expr::Ident(_) | Expr::FieldAccess(_) | Expr::Index(_) => match place_root(expr) {
                Some(root) => {
                    if let Some(held) = self
                        .sem()
                        .referenced_symbol(root.id)
                        .and_then(|def| self.flow.holders.get(&def))
                    {
                        owners.clone_from(held);
                    }
                }
                None => match expr {
                    Expr::FieldAccess(fa) => owners = self.owners_of(&fa.expr),
                    Expr::Index(ix) => owners = self.owners_of(&ix.expr),
                    _ => unreachable!("an identifier is a place root"),
                },
            },
            Expr::Unary(u) => match u.op {
                ast::UnaryOp::Ref => owners = self.reference_owners(&u.expr, false),
                ast::UnaryOp::MutRef => owners = self.reference_owners(&u.expr, true),
                _ => owners = self.owners_of(&u.expr),
            },
            Expr::Call(call) => {
                owners = self.owners_of(&call.callee);
                merge_owners(&mut owners, self.args_owners(expr, &call.args));
            }
            Expr::MethodCall(mc) => {
                owners = if self.sem().method_call_consumes_receiver(mc.id) {
                    self.owners_of(&mc.receiver)
                } else if self.sem().borrow_reaches_result(expr, Operand::Receiver) {
                    let writes = self.sem().method_call_takes_mut_receiver(mc.id);
                    self.reference_owners(&mc.receiver, writes)
                } else {
                    self.contents_owners(&mc.receiver)
                };
                merge_owners(&mut owners, self.args_owners(expr, &mc.args));
            }
            Expr::StaticMethodCall(smc) => owners = self.args_owners(expr, &smc.args),
            Expr::Cast(c) => owners = self.owners_of(&c.expr),
            Expr::TryOp(t) => owners = self.owners_of(&t.expr),
            Expr::Spread(inner, _) => owners = self.owners_of(inner),
            Expr::StructLiteral(s) => {
                for spread in &s.spreads {
                    merge_owners(&mut owners, self.owners_of(&spread.expr));
                }
                for field in &s.fields {
                    merge_owners(&mut owners, self.owners_of(&field.value));
                }
            }
            Expr::TupleLiteral(t) => {
                for el in &t.elements {
                    merge_owners(&mut owners, self.owners_of(el));
                }
            }
            Expr::TupleComprehension(c) => {
                owners = self.owners_of(&c.iterable);
                merge_owners(&mut owners, self.owners_of(&c.body));
            }
            // A hole is referenced where it stands.
            Expr::TaggedTemplate(t) => {
                for hole in t.template.interpolations() {
                    merge_owners(&mut owners, self.reference_owners(hole, false));
                }
            }
            Expr::Range(r) => {
                owners = self.owners_of(&r.start);
                merge_owners(&mut owners, self.owners_of(&r.end));
            }
            Expr::Block(block) => owners = self.block_owners(block),
            Expr::LabeledBlock(lb) => owners = self.labeled_block_owners(&lb.label, &lb.block),
            Expr::WithHandler(w) => owners = self.block_owners(&w.body),
            Expr::If(if_expr) => {
                owners = self.if_owners(&if_expr.then_block, if_expr.else_block.as_ref());
            }
            Expr::Match(m) => owners = self.arms_owners(&m.arms),
            Expr::Closure(c) => {
                owners = self.closure_owners.get(&c.id).cloned().unwrap_or_default();
            }
            Expr::Binary(_)
            | Expr::Assign(_)
            | Expr::CompoundAssign(_)
            | Expr::TemplateString(_)
            | Expr::Matches(_)
            | Expr::ComparisonChain(_)
            | Expr::Literal(_)
            | Expr::Error(_) => {}
        }
        owners
    }

    /// What the arguments of `call` hand its result. A borrow written as an
    /// argument counts only where the callee may hand it back; what the
    /// borrowed place holds counts either way.
    fn args_owners(&self, call: &Expr, args: &[Expr]) -> Owners {
        let mut owners = Owners::default();
        for (i, arg) in args.iter().enumerate() {
            let held = match arg {
                Expr::Unary(u)
                    if matches!(u.op, ast::UnaryOp::Ref | ast::UnaryOp::MutRef)
                        && !self.sem().borrow_reaches_result(call, Operand::Arg(i)) =>
                {
                    self.contents_owners(&u.expr)
                }
                _ => self.owners_of(arg),
            };
            merge_owners(&mut owners, held);
        }
        owners
    }

    /// What a reference to the place or temporary `expr` refers into; `writes`
    /// for a `&mut`.
    fn reference_owners(&self, expr: &Expr, writes: bool) -> Owners {
        self.borrow_owners(expr, Some(writes))
    }

    /// What the place or temporary `expr` holds references into, leaving out
    /// the place itself.
    fn contents_owners(&self, expr: &Expr) -> Owners {
        self.borrow_owners(expr, None)
    }

    /// `borrow` carries `writes` where a reference to `expr` itself is taken.
    fn borrow_owners(&self, expr: &Expr, borrow: Option<bool>) -> Owners {
        match expr {
            Expr::Unary(u) if u.op == ast::UnaryOp::Deref => self.owners_of(&u.expr),
            Expr::Index(ix) => self.borrow_owners(&ix.expr, borrow),
            Expr::FieldAccess(fa) if Self::place_path(expr).is_none() => {
                self.borrow_owners(&fa.expr, borrow)
            }
            _ => {
                let Some(place) = Self::place_path(expr) else {
                    return self.owners_of(expr);
                };
                let Some(def) = self.sem().referenced_symbol(place.root.id) else {
                    return Owners::default();
                };
                let mut owners = self.flow.holders.get(&def).cloned().unwrap_or_default();
                if let Some(writes) = borrow
                    && self.sem().local_type(def).is_some()
                {
                    owners.insert(
                        self.owner(def, place.owned_fields()),
                        OwnerSite {
                            name: place.root.name.clone(),
                            span: expr.span(),
                            affine: self.is_affine_local(def),
                            writes,
                        },
                    );
                }
                owners
            }
        }
    }

    /// What the value a block ends with refers into.
    fn block_owners(&self, block: &Block) -> Owners {
        match block.stmts.last() {
            Some(Stmt::Expr(e)) => self.owners_of(&e.expr),
            Some(Stmt::If(s)) => self.if_owners(&s.then_block, s.else_block.as_ref()),
            Some(Stmt::Match(m)) => self.arms_owners(&m.arms),
            Some(Stmt::LabeledBlock(lb)) => self.labeled_block_owners(&lb.label, &lb.block),
            _ => Owners::default(),
        }
    }

    fn if_owners(&self, then_block: &Block, else_block: Option<&Block>) -> Owners {
        let mut owners = self.block_owners(then_block);
        if let Some(eb) = else_block {
            merge_owners(&mut owners, self.block_owners(eb));
        }
        owners
    }

    fn arms_owners(&self, arms: &[ast::MatchArm]) -> Owners {
        let mut owners = Owners::default();
        for arm in arms {
            merge_owners(&mut owners, self.owners_of(&arm.body));
        }
        owners
    }

    fn labeled_block_owners(&self, label: &str, block: &Block) -> Owners {
        let mut owners = self.block_owners(block);
        if let Some(broken) = self.break_owners.get(label) {
            merge_owners(&mut owners, broken.clone());
        }
        owners
    }

    /// `pattern` binds afresh to what `value` refers into.
    fn bind_to(&mut self, pattern: &Pattern, value: &Expr) {
        self.bind_pattern(pattern);
        let owners = self.owners_of(value);
        self.hold_bindings(pattern, &owners);
    }

    /// Bind each of `pattern`'s bindings that may hold a reference to `owners`.
    fn hold_bindings(&mut self, pattern: &Pattern, owners: &Owners) {
        if owners.is_empty() {
            return;
        }
        let mut ids = Vec::new();
        for_each_pattern_binding(pattern, &mut |id| ids.push(id));
        for id in ids {
            if self
                .sem()
                .local_type(id)
                .is_some_and(|ty| self.sem().may_hold_reference(ty))
            {
                self.flow.holders.insert(id, owners.clone());
            }
        }
    }

    /// Whether `def` is a binding of this body rather than a capture.
    fn is_own_local(&self, def: AstId) -> bool {
        self.closure_locals
            .as_ref()
            .is_none_or(|locals| locals.contains(&def))
    }

    /// Refuse a returned value referring into an affine local of the body.
    fn refuse_returned(&mut self, value: &Expr) {
        if let Some(site) = self.affine_own_local(&self.owners_of(value)) {
            self.report(
                MoveError::ReturnsLocalReference { owner: site.name },
                site.span,
            );
        }
    }

    /// The site of an affine local of this body among `owners`, if any.
    fn affine_own_local(&self, owners: &Owners) -> Option<OwnerSite> {
        owners
            .iter()
            .find(|(owner, site)| site.affine && self.is_own_local(owner.def))
            .map(|(_, site)| site.clone())
    }

    /// At the end of the body, refuse each of `outside` that refers into an
    /// affine local of it.
    fn refuse_escapes(&mut self, outside: &[(AstId, String)]) {
        for (holder, holder_name) in outside {
            let Some(owners) = self.flow.holders.get(holder) else {
                continue;
            };
            if let Some(site) = self.affine_own_local(owners) {
                self.report(
                    MoveError::ReferenceEscapes {
                        holder: holder_name.clone(),
                        owner: site.name,
                    },
                    site.span,
                );
            }
        }
    }

    /// Runs `walk` in a scope of its own: the bindings it makes end with it.
    fn in_scope<R>(&mut self, walk: impl FnOnce(&mut Self) -> R) -> R {
        self.scopes.push(Vec::new());
        let result = walk(self);
        for id in self.scopes.pop().expect("the scope pushed above") {
            let instance = self.instance(id);
            self.ended.insert(id, instance);
        }
        result
    }

    fn visit_block(&mut self, block: &Block) -> bool {
        self.in_scope(|walker| {
            for stmt in &block.stmts {
                if walker.visit_stmt(stmt) {
                    return true;
                }
            }
            false
        })
    }

    fn visit_stmt(&mut self, stmt: &Stmt) -> bool {
        match stmt {
            Stmt::Let(l) => {
                let before = self.flow.clone();
                if let Some(value) = &l.value {
                    self.visit_value(value);
                }
                // A `let ... else` whose pattern fails consumes nothing; its
                // else block diverges, so the continuation keeps the state
                // the match left.
                if let Some(eb) = &l.else_block {
                    let matched = std::mem::replace(&mut self.flow, before);
                    self.visit_block(eb);
                    self.flow = matched;
                }
                match &l.value {
                    Some(value) => self.bind_to(&l.pattern, value),
                    None => self.bind_pattern(&l.pattern),
                }
                false
            }
            Stmt::Expr(e) => {
                self.visit_value(&e.expr);
                false
            }
            Stmt::Return(r) => {
                if let Some(value) = &r.value {
                    self.visit_value(value);
                    self.refuse_returned(value);
                }
                true
            }
            Stmt::TaskReturn(t) => {
                self.visit_value(&t.value);
                false
            }
            Stmt::If(s) => self.visit_if(&s.condition, &s.then_block, s.else_block.as_ref()),
            Stmt::While(s) => {
                self.visit_loop(&LoopHead::Condition(&s.condition), &s.body, None);
                false
            }
            Stmt::Loop(s) => {
                self.visit_loop(&LoopHead::None, &s.body, None);
                false
            }
            Stmt::For(s) => {
                self.in_scope(|walker| {
                    if let Some(init) = &s.init {
                        walker.visit_stmt(init);
                    }
                    let head = match &s.condition {
                        Some(cond) => LoopHead::Condition(cond),
                        None => LoopHead::None,
                    };
                    walker.visit_loop(&head, &s.body, s.update.as_ref());
                });
                false
            }
            // The loop owns what it walks (WEP: Affine Types).
            Stmt::ForOf(s) => {
                self.visit_value(&s.iterable);
                let owners = self.owners_of(&s.iterable);
                self.visit_loop(&LoopHead::Binding(&s.binding, owners), &s.body, None);
                false
            }
            Stmt::Match(m) => self.visit_match(&m.expr, &m.arms),
            Stmt::Break(b) => {
                if let Some(value) = &b.value {
                    self.visit_value(value);
                    if let Some(label) = &b.label {
                        let owners = self.owners_of(value);
                        merge_owners(self.break_owners.entry(label.clone()).or_default(), owners);
                    }
                }
                true
            }
            Stmt::Assert(a) => {
                self.visit_expr(&a.condition);
                if let Some(msg) = &a.message {
                    self.visit_expr(msg);
                }
                false
            }
            Stmt::LabeledBlock(lb) => self.visit_block(&lb.block),
            Stmt::Continue(_) => true,
            Stmt::Item(item) => {
                self.checker.check_item(item);
                false
            }
            Stmt::Error(_) => false,
        }
    }

    /// A loop as two iterations: the first records what it moves, and the
    /// second reports what reads a value the first moved. Each iteration runs
    /// the head, the body, then `update`, as a C-style `for` does.
    /// The loop may also run no iteration, so what it leaves joins the state
    /// it entered with.
    fn visit_loop(&mut self, head: &LoopHead, body: &Block, update: Option<&Expr>) {
        let entry = self.flow.clone();
        self.suppress += 1;
        self.visit_iteration(head, body, update);
        self.suppress -= 1;
        self.visit_iteration(head, body, update);
        self.flow.merge(entry);
    }

    fn visit_iteration(&mut self, head: &LoopHead, body: &Block, update: Option<&Expr>) {
        self.in_scope(|walker| {
            match head {
                LoopHead::None => {}
                LoopHead::Condition(condition) => walker.visit_condition(condition),
                LoopHead::Binding(pattern, owners) => {
                    walker.bind_pattern(pattern);
                    walker.hold_bindings(pattern, owners);
                }
            }
            walker.visit_block(body);
            if let Some(update) = update {
                walker.visit_expr(update);
            }
        });
    }

    /// The bindings `pattern` makes hold new values, none of them moved, and
    /// none of them referring into anything yet.
    fn bind_pattern(&mut self, pattern: &Pattern) {
        for_each_pattern_binding(pattern, &mut |id| {
            self.flow.moved.retain(|(def, _), _| *def != id);
            self.flow.holders.shift_remove(&id);
            *self.instances.entry(id).or_default() += 1;
            if let Some(scope) = self.scopes.last_mut() {
                scope.push(id);
            }
            if let Some(locals) = &mut self.closure_locals {
                locals.insert(id);
            }
        });
    }

    /// An `if` whose `let`s consume their scrutinees on the matched path only:
    /// a refutable pattern that fails consumes nothing, so the `else` still
    /// holds them.
    fn visit_if(
        &mut self,
        condition: &Condition,
        then_block: &Block,
        else_block: Option<&Block>,
    ) -> bool {
        let before = self.flow.clone();
        self.suppress += 1;
        self.visit_condition_reading(condition);
        self.suppress -= 1;
        let unmatched = std::mem::replace(&mut self.flow, before);
        let then_div = self.in_scope(|walker| {
            walker.visit_condition(condition);
            walker.visit_block(then_block)
        });
        let then_flow = std::mem::replace(&mut self.flow, unmatched);
        let else_div = match else_block {
            Some(eb) => self.visit_block(eb),
            None => false,
        };
        if !then_div {
            self.flow.merge(then_flow);
        }
        else_block.is_some() && then_div && else_div
    }

    fn visit_match(&mut self, scrutinee: &Expr, arms: &[ast::MatchArm]) -> bool {
        self.visit_value(scrutinee);
        let owners = self.owners_of(scrutinee);
        let base = std::mem::take(&mut self.flow);
        let mut merged = base.clone();
        let mut all_diverge = true;
        for arm in arms {
            self.flow.clone_from(&base);
            let diverged = self.in_scope(|walker| {
                walker.bind_pattern(&arm.pattern);
                walker.hold_bindings(&arm.pattern, &owners);
                if let Some(guard) = &arm.guard {
                    let saved = walker.guard_bindings.clone();
                    for_each_pattern_binding(&arm.pattern, &mut |id| {
                        walker.guard_bindings.insert(id);
                    });
                    walker.visit_expr(guard);
                    walker.guard_bindings = saved;
                }
                walker.visit_expr_diverges(&arm.body)
            });
            if !diverged {
                all_diverge = false;
                merged.merge(std::mem::take(&mut self.flow));
            }
        }
        self.flow = merged;
        all_diverge
    }

    fn visit_expr_diverges(&mut self, expr: &Expr) -> bool {
        match expr {
            Expr::Block(block) => self.visit_block(block),
            Expr::LabeledBlock(lb) => self.visit_block(&lb.block),
            other => {
                self.visit_value(other);
                false
            }
        }
    }

    /// A condition on the path where its `let`s matched: each consumes its
    /// scrutinee and binds its pattern afresh.
    fn visit_condition(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    match element {
                        ConditionElement::Let { pattern, expr, .. } => {
                            self.visit_value(expr);
                            self.bind_to(pattern, expr);
                        }
                        ConditionElement::Expr(e) => self.visit_expr(e),
                    }
                }
            }
        }
    }

    /// A condition on the path where it failed. A `let` that failed consumed
    /// nothing, but one with an element after it may have matched before that
    /// element failed, and then its scrutinee is in a binding.
    fn visit_condition_reading(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                let last = elements.len() - 1;
                for (i, element) in elements.iter().enumerate() {
                    match element {
                        ConditionElement::Let { expr, .. } if i < last => self.visit_value(expr),
                        _ => self.visit_expr(element.expr()),
                    }
                }
            }
        }
    }
}

/// The binding a place of `expr` is rooted in, through fields and elements.
fn place_root(expr: &Expr) -> Option<&IdentExpr> {
    match expr {
        Expr::Ident(ident) => Some(ident),
        Expr::FieldAccess(fa) => place_root(&fa.expr),
        Expr::Index(ix) => place_root(&ix.expr),
        _ => None,
    }
}

/// The binding a write through `expr` starts from: through fields, elements,
/// references, and the receivers of calls that may hand back a reference
/// into them.
fn write_root(expr: &Expr) -> Option<&IdentExpr> {
    match expr {
        Expr::Ident(ident) => Some(ident),
        Expr::FieldAccess(fa) => write_root(&fa.expr),
        Expr::Index(ix) => write_root(&ix.expr),
        Expr::Unary(u) => write_root(&u.expr),
        Expr::Cast(c) => write_root(&c.expr),
        Expr::MethodCall(mc) => write_root(&mc.receiver),
        _ => None,
    }
}

/// Whether one field path holds the other: they agree as far as the shorter
/// one goes.
fn paths_overlap(moved: &[String], read: &[&str]) -> bool {
    moved.iter().zip(read).all(|(m, r)| m == r)
}
