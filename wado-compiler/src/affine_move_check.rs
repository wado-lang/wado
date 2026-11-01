//! The affine move check (WEP: Affine Types): a body reads an affine value at
//! most once, and a named declaration says whether it is affine. It runs on the
//! `Semantics` layer, over every module, and needs no instantiation: a generic
//! body is checked once, with its `affine` parameters affine.

use std::rc::Rc;

use crate::ast::{
    self, AssignExpr, AssignTarget, AstId, Block, ClosureExpr, Condition, ConditionElement, Expr,
    Function, IdentExpr, ImplBlock, Item, Pattern, SelfKind, Stmt, for_each_pattern_binding,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::compiler_item::CompilerItem;
use crate::divergence::{self, ExprTypes};
use crate::elaborator::sem::types::{DestructuringFallback, FallbackSite};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::ownership::{self, BodyFacts, Modes, OperandMode, PlacePath};
use crate::semantics::{Destination, Operand, Semantics};
use crate::symbol::SymbolKind;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;
use crate::unparse::unparse_type_into;

#[derive(Debug, Clone)]
pub struct AffineMoveError {
    kind: MoveError,
    span: Span,
    module: String,
    /// Why the destructuring assignment the error sits in copies.
    note: Option<String>,
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
    /// The `self` of a `Drop::drop` body moved as a whole.
    DropSelfMove { type_name: String },
    /// `..base` over a type that implements `Drop`.
    DropSpread { type_name: String },
    /// A value read after a local it refers into moved, dropped, went to a
    /// `&mut` use other than through the value, or took an assignment that
    /// replaced what the value names.
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

impl From<AffineMoveError> for Diagnostic {
    fn from(e: AffineMoveError) -> Self {
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
            MoveError::DropSelfMove { type_name } => (
                Code::TypeMismatch,
                format!(
                    "the `drop` of `{type_name}` cannot move `self` as a whole: wherever it went, it would drop and run `drop` again"
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
                    Gone::Replaced(at) => format!(
                        "`{holder}` refers into `{owner}`, which the assignment at {}:{} replaced",
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
        let message = match e.note {
            Some(note) => format!("{message}\n  note: {note}"),
            None => message,
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
pub fn check_affine_moves_semantic(sem: &Semantics) -> Vec<AffineMoveError> {
    let drops = Drops::of(sem);
    let fallbacks = sem.destructuring_fallbacks();
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if sem.snapshot_modules.contains(src) {
            continue;
        }
        let mut checker = Checker {
            sem,
            module: src,
            drops: &drops,
            out: &mut out,
            fallbacks: &fallbacks,
            in_fallback: None,
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

struct Checker<'a> {
    sem: &'a Semantics,
    module: &'a ModuleSource,
    drops: &'a Drops,
    out: &'a mut Vec<AffineMoveError>,
    /// [`Semantics::destructuring_fallbacks`].
    fallbacks: &'a IndexMap<Span, &'a FallbackSite>,
    /// The falling-back destructuring assignment whose right side is being
    /// walked, where it has a cause to note.
    in_fallback: Option<NotedFallback>,
}

struct NotedFallback {
    /// The assignment's span, its first token.
    at: Span,
    /// [`FallbackSite::target_reads`].
    target_reads: Vec<Span>,
    note: String,
}

impl<'a> Checker<'a> {
    fn push(&mut self, kind: MoveError, span: Span) {
        // A copy the fallback makes is a read of a target: a move out of a
        // reference, or a second read of one the right side moved already,
        // between the assignment's first token and this read.
        let note = self
            .in_fallback
            .as_ref()
            .filter(|f| f.target_reads.iter().any(|read| read.encloses(&span)))
            .filter(|f| match &kind {
                MoveError::MoveOutOfReference { .. } => true,
                MoveError::UseAfterMove { moved_at, .. } => f.at.merge(&span).encloses(moved_at),
                _ => false,
            })
            .map(|f| f.note.clone());
        self.out.push(AffineMoveError {
            kind,
            span,
            module: self.module.source_path(),
            note,
        });
    }

    fn check_item(&mut self, item: &Item) {
        self.check_affine_declaration(item);
        match item {
            Item::Function(func) => self.check_function(func, false),
            Item::Impl(b) => {
                self.check_impl_trait(b);
                let drop_impl = self.drops.implemented_by(b, self.sem);
                for method in &b.methods {
                    self.check_function(method, drop_impl);
                }
            }
            Item::Trait(t) => {
                for method in &t.methods {
                    self.check_function(method, false);
                }
            }
            Item::Interface(i) => {
                for method in &i.methods {
                    self.check_function(method, false);
                }
            }
            Item::Test(test) => self.check_body(&test.body, &[], None, Vec::new()),
            _ => {}
        }
    }

    fn check_function(&mut self, func: &Function, drop_impl: bool) {
        let Some(body) = &func.body else {
            return;
        };
        // A body no walk reached records no types, so nothing in it is known
        // to be affine.
        let Some(scope) = self.sem.affine_params(func.id) else {
            assert!(
                !self.sem.is_complete(),
                "complete semantics walk every body"
            );
            return;
        };
        let drop_self = func
            .params
            .iter()
            .find(|p| drop_impl && matches!(p.self_kind, SelfKind::Value))
            .map(|p| p.id);
        let outside = func
            .params
            .iter()
            .filter(|p| self.is_mut_ref_local(p.id))
            .map(|p| (p.id, p.name.clone()))
            .collect();
        self.check_body(body, scope, drop_self, outside);
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
        affine_params: &'a [TypeId],
        drop_self: Option<AstId>,
        outside: Vec<(AstId, String)>,
    ) {
        let facts = SemFacts {
            sem: self.sem,
            affine_params,
        };
        let modes = ownership::body_modes(body, &facts);
        let mut walker = MoveWalker::new(self, Rc::new(modes), affine_params, drop_self);
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
    fn check_impl_trait(&mut self, b: &ImplBlock) {
        let Some(trait_type) = &b.trait_type else {
            return;
        };
        let Some(Item::Trait(t)) = self.sem.implemented(b) else {
            return;
        };
        let mut type_name = String::new();
        unparse_type_into(&b.ty, &mut type_name);
        if Some(t.id) == self.drops.trait_id {
            if !self.impl_self_is_affine(b) {
                self.push(MoveError::DropOnCopyable { type_name }, trait_type.span());
            }
            return;
        }
        if t.is_affine || !self.impl_self_is_affine(b) {
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
    fn impl_self_is_affine(&self, b: &ImplBlock) -> bool {
        let mut mentioned = Vec::new();
        b.ty.mentioned_names(&mut mentioned);
        let names_affine_param = b
            .type_params
            .iter()
            .any(|p| p.is_affine && mentioned.contains(&p.name));
        names_affine_param
            || self
                .sem
                .declaration_of(&b.ty)
                .and_then(|decl| self.sem.types.type_of_symbol(&decl.id()))
                .is_some_and(|ty| self.sem.is_affine(ty, &[]))
    }
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

impl LoopHead<'_> {
    /// Whether the head never ends the loop, leaving that to a jump.
    fn is_endless(&self) -> bool {
        match self {
            Self::None => true,
            Self::Condition(condition) => condition.is_always_true(),
            Self::Binding(..) => false,
        }
    }
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
    /// An assignment to a place holding the object, which dropped it.
    Replaced(Span),
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
    /// Why the object the value names left the local, once an assignment
    /// refilled or replaced the place holding it.
    gone: Option<Gone>,
}

impl OwnerSite {
    /// Whether a reference so taken can raise anything: a move or a drop ends
    /// only what reaches an affine local, and only a write through the value
    /// disturbs one or stores into it. A shared reference into a local that
    /// does not move is neither, and a generated pass hands thousands of them
    /// to the one `&mut` backend every value then refers into.
    fn matters(&self) -> bool {
        self.affine || self.writes
    }
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
        let gone = site.gone;
        let kept = into.entry(owner).or_insert(site);
        if kept.gone.is_none() {
            kept.gone = gone;
        }
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

/// What a body's modes read: the types the elaborator recorded, and the body's
/// `affine` type parameters.
pub(crate) struct SemFacts<'s> {
    pub(crate) sem: &'s Semantics,
    pub(crate) affine_params: &'s [TypeId],
}

impl BodyFacts for SemFacts<'_> {
    fn consumes_receiver(&self, call: AstId) -> bool {
        self.sem.method_call_consumes_receiver(call)
    }

    fn indexes_tuple(&self, base: &Expr) -> bool {
        self.sem.expression_type(base.id()).is_some_and(|ty| {
            let types = &self.sem.types;
            matches!(types.get(types.peel_refs(ty)), ResolvedType::GenericInstance { def, .. }
                if types.is_tuple_def(*def))
        })
    }

    fn is_affine(&self, expr: AstId) -> bool {
        self.sem
            .expression_type(expr)
            .is_some_and(|ty| self.sem.is_affine(ty, self.affine_params))
    }

    fn names_variable(&self, ident: &IdentExpr) -> bool {
        self.sem
            .referenced_symbol(ident.id)
            .and_then(|def| self.sem.symbol_at(def))
            .is_some_and(ownership::is_variable)
    }
}

struct MoveWalker<'a, 'c> {
    checker: &'c mut Checker<'a>,
    /// The body's modes, which a closure inside it shares.
    modes: Rc<Modes>,
    affine_params: &'a [TypeId],
    /// The `self` of a `Drop::drop` body, which may move its fields out.
    drop_self: Option<AstId>,
    /// The scrutinees a pattern takes apart, so no binding holds them whole.
    decomposed: IndexSet<AstId>,
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
    /// The state each enclosing loop's `break`s and `continue`s carry to where
    /// they jump, innermost last.
    loops: Vec<LoopJumps>,
    /// The state each enclosing labeled block's `break`s carry to its end,
    /// innermost last.
    labels: Vec<(String, Option<Flow>)>,
    suppress: u32,
}

#[derive(Default)]
struct LoopJumps {
    breaks: Option<Flow>,
    continues: Option<Flow>,
}

/// The receiver a call spells, if a method call, and its arguments.
fn call_operands(call: &Expr) -> (Option<&ast::MethodCallExpr>, &[Expr]) {
    match call {
        Expr::Call(c) => (None, &c.args),
        Expr::MethodCall(mc) => (Some(mc), &mc.args),
        Expr::StaticMethodCall(smc) => (None, &smc.args),
        _ => unreachable!("only a call has operands"),
    }
}

/// `flow` reaching a point other paths may reach too.
fn reach(point: &mut Option<Flow>, flow: Flow) {
    match point {
        Some(joined) => joined.merge(flow),
        None => *point = Some(flow),
    }
}

impl<'a, 'c> MoveWalker<'a, 'c> {
    fn new(
        checker: &'c mut Checker<'a>,
        modes: Rc<Modes>,
        affine_params: &'a [TypeId],
        drop_self: Option<AstId>,
    ) -> Self {
        MoveWalker {
            checker,
            modes,
            affine_params,
            drop_self,
            decomposed: IndexSet::default(),
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
            loops: Vec::new(),
            labels: Vec::new(),
            suppress: 0,
        }
    }

    /// A walker of a body nested in this one, which sees the state here.
    fn sub_walker(&mut self) -> MoveWalker<'a, '_> {
        let mut inner = MoveWalker::new(
            &mut *self.checker,
            Rc::clone(&self.modes),
            self.affine_params,
            self.drop_self,
        );
        inner.flow.clone_from(&self.flow);
        inner.guard_bindings.clone_from(&self.guard_bindings);
        inner.instances.clone_from(&self.instances);
        inner.ended.clone_from(&self.ended);
        inner.disturbances.clone_from(&self.disturbances);
        inner.suppress = self.suppress;
        inner
    }
}

impl ExprTypes for MoveWalker<'_, '_> {
    fn is_never(&self, expr: &Expr) -> bool {
        self.sem()
            .expression_type(expr.id())
            .is_some_and(|ty| self.sem().types.is_never(ty))
    }
}

impl MoveWalker<'_, '_> {
    fn sem(&self) -> &Semantics {
        self.checker.sem
    }

    fn facts(&self) -> SemFacts<'_> {
        SemFacts {
            sem: self.sem(),
            affine_params: self.affine_params,
        }
    }

    fn is_affine(&self, type_id: TypeId) -> bool {
        self.sem().is_affine(type_id, self.affine_params)
    }

    fn expr_is_affine(&self, id: AstId) -> bool {
        self.facts().is_affine(id)
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

    fn place_path<'e>(&self, expr: &'e Expr) -> Option<PlacePath<'e>> {
        ownership::place_path(expr, &self.facts())
    }

    fn field_step<'e>(&self, expr: &'e Expr) -> Option<(&'e Expr, &'e str)> {
        ownership::field_step(expr, &self.facts())
    }

    /// Whether `expr` is a container's element, or a field of one at any
    /// depth.
    fn roots_in_element(&self, expr: &Expr) -> bool {
        match self.field_step(expr) {
            Some((base, _)) => self.roots_in_element(base),
            None => matches!(expr, Expr::Index(_)),
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
        self.field_step(expr).is_some_and(|(base, _)| {
            matches!(base, Expr::Unary(u) if u.op == ast::UnaryOp::Deref)
                || self.is_reference(base)
                || self.path_through_reference(base)
        })
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
                .then(|| site.gone.or_else(|| self.owner_gone(holder, owner)))
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
        let path: Vec<String> = self
            .place_path(expr)
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
        if self.is_drop_self(expr) && !self.decomposed.contains(&expr.id()) {
            let type_name = self.type_name_of(expr);
            self.report(MoveError::DropSelfMove { type_name }, place.root.span);
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
        self.sem().types.name(ty)
    }

    /// Refuse a move of the field `expr` names out of a holder whose type
    /// implements `Drop`, at any depth. The `self` of a `Drop::drop` body is the
    /// one holder a field may leave: what `drop` leaves behind drops field by
    /// field.
    fn refuse_move_out_of_drop(&mut self, mut expr: &Expr) {
        while let Some((holder, _)) = self.field_step(expr) {
            if !self.is_drop_self(holder) && self.implements_drop(holder) {
                let type_name = self.type_name_of(holder);
                self.report(MoveError::DropPartialMove { type_name }, expr.span());
            }
            expr = holder;
        }
    }

    fn implements_drop(&self, expr: &Expr) -> bool {
        self.sem()
            .expression_type(expr.id())
            .is_some_and(|ty| self.type_implements_drop(ty))
    }

    /// Whether `ty`, or the type a newtype over it represents, implements
    /// `Drop`: a newtype inherits its base's.
    fn type_implements_drop(&self, ty: TypeId) -> bool {
        let types = &self.sem().types;
        let head = types.representation_head(ty);
        types.nominal_def(head).is_some_and(|def| {
            self.checker
                .drops
                .implementors
                .contains(&types.defs().ast_id(def))
        })
    }

    /// Refuse a by-value `pattern` that moves an affine part out of a value
    /// whose type implements `Drop`, at any depth, as
    /// [`Self::refuse_move_out_of_drop`] refuses the field move. `scrutinee`
    /// is what the pattern matches, where an expression names it: the `self`
    /// of a `Drop::drop` body may still give up its own fields.
    fn refuse_destructured_drop(&mut self, pattern: &Pattern, scrutinee: Option<&Expr>) {
        let own_self = scrutinee.is_some_and(|expr| self.is_drop_self(expr));
        self.refuse_destructured_drop_at(pattern, own_self);
    }

    fn is_drop_self(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Ident(id) if self.drop_self.is_some_and(
            |param| self.sem().referenced_symbol(id.id) == Some(param)
        ))
    }

    fn refuse_destructured_drop_at(&mut self, pattern: &Pattern, own_self: bool) {
        match pattern {
            Pattern::Struct { fields, span, .. } => {
                if let Some(first) = fields.first()
                    && !own_self
                    && fields.iter().any(|field| self.binds_affine(&field.pattern))
                {
                    self.refuse_destructured_part(first.id, *span);
                }
                for field in fields {
                    self.refuse_destructured_drop_at(&field.pattern, false);
                }
            }
            Pattern::Variant {
                name_id,
                bindings,
                span,
                ..
            } => {
                if !own_self && bindings.iter().any(|binding| self.binds_affine(binding)) {
                    self.refuse_destructured_part(*name_id, *span);
                }
                for binding in bindings {
                    self.refuse_destructured_drop_at(binding, false);
                }
            }
            Pattern::Tuple(elements, _) => {
                for element in elements {
                    self.refuse_destructured_drop_at(element, false);
                }
            }
            Pattern::Or(alternatives) => {
                for alternative in alternatives {
                    self.refuse_destructured_drop_at(alternative, own_self);
                }
            }
            Pattern::Typed { pattern, .. } => self.refuse_destructured_drop_at(pattern, own_self),
            Pattern::Ident { .. }
            | Pattern::MutIdent { .. }
            | Pattern::Literal(_)
            | Pattern::Wildcard
            | Pattern::Range { .. }
            | Pattern::Error(_) => {}
        }
    }

    /// Refuse the part a pattern node at `span` moves out of the type it takes
    /// apart, which the walk recorded at `site`, where that type implements
    /// `Drop`.
    fn refuse_destructured_part(&mut self, site: AstId, span: Span) {
        let Some(ty) = self.sem().destructured_type(site) else {
            return;
        };
        if self.type_implements_drop(ty) {
            let type_name = self.sem().types.name(ty);
            self.report(MoveError::DropPartialMove { type_name }, span);
        }
    }

    /// Whether `pattern` binds an affine value by value.
    fn binds_affine(&self, pattern: &Pattern) -> bool {
        let mut affine = false;
        for_each_pattern_binding(pattern, &mut |id| affine |= self.is_affine_local(id));
        affine
    }

    /// An assignment at `at` to `place` refills the moves inside it and drops
    /// the old objects below it. A reference taken before into either names
    /// an object the local no longer holds: one into a refilled place at or
    /// below where it moved, or one below `place`. One above a move names an
    /// object the assignment writes in place.
    fn assign_place(&mut self, place: &PlacePath<'_>, at: Span) {
        let Some(def) = self.sem().referenced_symbol(place.root.id) else {
            return;
        };
        let mut refilled = Vec::new();
        self.flow.moved.retain(|(moved_def, moved_path), span| {
            let inside = *moved_def == def
                && moved_path.len() >= place.fields.len()
                && paths_overlap(moved_path, &place.fields);
            if inside {
                refilled.push((moved_path.clone(), *span));
            }
            !inside
        });
        let instance = self.instance(def);
        for (owner, site) in self
            .flow
            .holders
            .values_mut()
            .flat_map(|owners| owners.iter_mut())
        {
            if owner.def != def || owner.instance != instance || site.gone.is_some() {
                continue;
            }
            let path: Vec<&str> = owner.path.iter().map(String::as_str).collect();
            site.gone = refilled
                .iter()
                .find(|(moved, _)| moved.len() <= path.len() && paths_overlap(moved, &path))
                .map(|(_, span)| Gone::Moved(*span))
                .or_else(|| {
                    (owner.path.len() > place.fields.len()
                        && paths_overlap(&owner.path, &place.fields))
                    .then_some(Gone::Replaced(at))
                });
        }
    }

    /// Visit `expr` in the mode its position has.
    fn visit_at(&mut self, expr: &Expr) {
        self.visit(expr, self.modes.of(expr));
    }

    /// Visit `expr` in a position of `mode`. A place the modes move moves
    /// unless `mode` reads it, and is read anywhere else; drop elaboration
    /// takes ownership at the same places.
    fn visit(&mut self, expr: &Expr, mode: OperandMode) {
        let consumes = mode == OperandMode::Consume;
        if let Some(place) = self.place_path(expr) {
            if consumes && self.modes.moves(expr, &self.facts()) {
                self.consume(expr, &place);
            } else {
                self.read(&place);
            }
            return;
        }
        if consumes
            && self.expr_is_affine(expr.id())
            && let Some(base) = self.out_of_reference(expr)
        {
            self.visit(base, OperandMode::Borrow);
            let type_name = self.type_name_of(expr);
            self.report(MoveError::MoveOutOfReference { type_name }, expr.span());
            return;
        }
        match expr {
            Expr::Block(block) => {
                self.visit_block(block);
            }
            Expr::LabeledBlock(lb) => {
                self.visit_labeled_block(&lb.label, &lb.block);
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
            Expr::Closure(closure) => self.visit_closure(closure),
            Expr::Assign(a) => self.visit_assign(a),
            Expr::Call(call) => {
                self.visit_operands(expr);
                self.store_through_call(expr);
                // A closure writes through to what it captures.
                if matches!(call.callee, Expr::Ident(_)) {
                    self.disturb(&call.callee);
                }
            }
            Expr::MethodCall(_) | Expr::StaticMethodCall(_) => {
                self.visit_operands(expr);
                self.store_through_call(expr);
            }
            Expr::WithHandler(w) => {
                self.visit_operands(expr);
                self.visit_block(&w.body);
            }
            Expr::StructLiteral(s) => {
                if let Some(spread) = s.spreads.first()
                    && self.implements_drop(expr)
                {
                    let type_name = self.type_name_of(expr);
                    self.report(MoveError::DropSpread { type_name }, spread.expr.span());
                }
                self.visit_operands(expr);
            }
            Expr::Unary(_)
            | Expr::Binary(_)
            | Expr::CompoundAssign(_)
            | Expr::ComparisonChain(_)
            | Expr::Index(_)
            | Expr::FieldAccess(_)
            | Expr::Cast(_)
            | Expr::Spread(..)
            | Expr::TryOp(_)
            | Expr::TupleLiteral(_)
            | Expr::TupleComprehension(_)
            | Expr::TemplateString(_)
            | Expr::TaggedTemplate(_)
            | Expr::Range(_)
            | Expr::Matches(_)
            | Expr::Ident(_)
            | Expr::Literal(_)
            | Expr::Error(_) => self.visit_operands(expr),
        }
    }

    /// The base an affine value read by value from `expr` would leave its
    /// owner in: a reference, or a container whose `&self` hands out an
    /// element.
    fn out_of_reference<'e>(&self, expr: &'e Expr) -> Option<&'e Expr> {
        if let Expr::Unary(u) = expr
            && u.op == ast::UnaryOp::Deref
        {
            return Some(&u.expr);
        }
        if let Some((base, _)) = self.field_step(expr)
            && (self.path_through_reference(expr) || self.roots_in_element(base))
        {
            return Some(base);
        }
        self.roots_in_element(expr).then_some(expr)
    }

    /// Visit each operand of `expr` in the mode of its position.
    fn visit_operands(&mut self, expr: &Expr) {
        for operand in ownership::operands(expr) {
            self.visit_at(operand);
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
        let mut inner = self.sub_walker();
        inner.closure_locals = Some(locals);
        inner.visit_at(&closure.body);
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
                gone: None,
            };
            if site.matters() {
                owners.insert(self.owner(def, Vec::new()), site);
            }
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
        let destructuring = matches!(a.target, AssignTarget::Destructuring { .. });
        match self.checker.fallbacks.get(&a.span) {
            // An `index_swap` in every walk reads no element out: the right
            // side names the places the targets write.
            None if destructuring => {}
            site => {
                let noted = site.and_then(|site| match &site.cause {
                    Some(DestructuringFallback::ReferenceReadable) | None => None,
                    Some(cause) => Some(NotedFallback {
                        at: a.span,
                        target_reads: site.target_reads.clone(),
                        note: cause.note(),
                    }),
                });
                let enclosing = std::mem::replace(&mut self.checker.in_fallback, noted);
                self.visit_at(&a.value);
                self.checker.in_fallback = enclosing;
            }
        }
        let places = a.places();
        let owners: Vec<Owners> = match (&a.value, &a.target) {
            (Expr::TupleLiteral(value), AssignTarget::Destructuring { .. })
                if value.elements.len() == places.len() =>
            {
                value
                    .elements
                    .iter()
                    .map(|element| self.owners_of(element))
                    .collect()
            }
            _ => vec![self.owners_of(&a.value); places.len()],
        };
        for (place, owners) in places.iter().zip(owners) {
            self.write_place(place, owners);
        }
    }

    fn write_place(&mut self, place: &Expr, owners: Owners) {
        match place {
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
        match self.place_path(place) {
            Some(path) => self.assign_place(&path, place.span()),
            None => self.visit_at(place),
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
    fn write_targets(&self, target: &Expr) -> IndexSet<AstId> {
        let mut targets = IndexSet::default();
        let Some(root) = write_root(target) else {
            return targets;
        };
        let Some(def) = self.sem().referenced_symbol(root.id) else {
            return targets;
        };
        targets.insert(def);
        let mut at = 0;
        while at < targets.len() {
            if let Some(owners) = self.flow.holders.get(&targets[at]) {
                for owner in owners.keys() {
                    targets.insert(owner.def);
                }
            }
            at += 1;
        }
        targets
    }

    /// A call may store what its operands hand it through each `&mut` it
    /// takes.
    fn store_through_call(&mut self, call: &Expr) {
        let (method, args) = call_operands(call);
        let mut through: Vec<(Operand, &Expr)> = Vec::new();
        if let Some(mc) = method
            && self.sem().method_call_takes_mut_receiver(mc.id)
        {
            through.push((Operand::Receiver, &mc.receiver));
        }
        for (i, arg) in args.iter().enumerate() {
            if self.is_mut_ref(arg) {
                through.push((Operand::Arg(i), arg));
            }
        }
        let stores: Vec<(IndexSet<AstId>, Owners)> = through
            .iter()
            .map(|(operand, target)| {
                let owners = self.operands_owners(call, Destination::Through(*operand));
                (self.write_targets(target), owners)
            })
            .collect();
        for (_, target) in &through {
            self.disturb(target);
        }
        for (targets, owners) in stores {
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
                merge_owners(&mut owners, self.operands_owners(expr, Destination::Result));
            }
            Expr::MethodCall(_) | Expr::StaticMethodCall(_) => {
                owners = self.operands_owners(expr, Destination::Result);
            }
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

    /// What the operands of `call` hand `destination`. A borrow written as an
    /// operand counts only where the callee may put it there; what the
    /// borrowed place holds counts either way.
    fn operands_owners(&self, call: &Expr, destination: Destination) -> Owners {
        let reaches = |operand| self.sem().borrow_reaches(call, operand, destination);
        let (method, args) = call_operands(call);
        let mut owners = Owners::default();
        if let Some(mc) = method {
            let held = if self.sem().method_call_consumes_receiver(mc.id) {
                self.owners_of(&mc.receiver)
            } else if reaches(Operand::Receiver) {
                let writes = self.sem().method_call_takes_mut_receiver(mc.id);
                self.reference_owners(&mc.receiver, writes)
            } else {
                self.contents_owners(&mc.receiver)
            };
            merge_owners(&mut owners, held);
        }
        for (i, arg) in args.iter().enumerate() {
            let held = match arg {
                Expr::Unary(u)
                    if matches!(u.op, ast::UnaryOp::Ref | ast::UnaryOp::MutRef)
                        && !reaches(Operand::Arg(i)) =>
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
            Expr::FieldAccess(fa) if self.place_path(expr).is_none() => {
                self.borrow_owners(&fa.expr, borrow)
            }
            _ => {
                let Some(place) = self.place_path(expr) else {
                    return self.owners_of(expr);
                };
                let Some(def) = self.sem().referenced_symbol(place.root.id) else {
                    return Owners::default();
                };
                let mut owners = self.flow.holders.get(&def).cloned().unwrap_or_default();
                if let Some(writes) = borrow
                    && self.sem().local_type(def).is_some()
                {
                    let site = OwnerSite {
                        name: place.root.name.clone(),
                        span: expr.span(),
                        affine: self.is_affine_local(def),
                        writes,
                        gone: None,
                    };
                    if site.matters() {
                        owners.insert(self.owner(def, place.owned_fields()), site);
                    }
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
        self.refuse_destructured_drop(pattern, Some(value));
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
            let mut stmts = block.stmts.iter();
            let diverged = stmts.by_ref().any(|stmt| walker.visit_stmt(stmt));
            if diverged {
                walker.visit_unreachable(|dead| {
                    for stmt in stmts {
                        dead.visit_stmt(stmt);
                    }
                });
            }
            diverged
        })
    }

    /// Checks code no path reaches, from the state where the path left. What it
    /// does reaches nothing: `walk` runs a walker of its own, which reports
    /// into the checker and is dropped, and its jumps land nowhere.
    fn visit_unreachable(&mut self, walk: impl FnOnce(&mut MoveWalker<'_, '_>)) {
        let closure_locals = self.closure_locals.clone();
        let captures = self.captures.clone();
        let disturbed_captures = self.disturbed_captures.clone();
        let closure_owners = self.closure_owners.clone();
        let break_owners = self.break_owners.clone();
        let mut dead = self.sub_walker();
        dead.closure_locals = closure_locals;
        dead.captures = captures;
        dead.disturbed_captures = disturbed_captures;
        dead.closure_owners = closure_owners;
        dead.break_owners = break_owners;
        walk(&mut dead);
    }

    /// Walks `stmt`, answering whether control never reaches past it.
    fn visit_stmt(&mut self, stmt: &Stmt) -> bool {
        let walked = self.walk_stmt(stmt);
        walked || divergence::stmt_head_always_exits(self, stmt)
    }

    /// Answers the divergence of the paths `stmt` chooses between; what it
    /// evaluates first is [`divergence::stmt_head_always_exits`]'s to answer.
    fn walk_stmt(&mut self, stmt: &Stmt) -> bool {
        match stmt {
            Stmt::Let(l) => {
                let before = self.flow.clone();
                if let Some(value) = &l.value {
                    self.note_decomposed(value, [&l.pattern]);
                    self.visit_at(value);
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
                self.visit_at(&e.expr);
                false
            }
            Stmt::Return(r) => {
                if let Some(value) = &r.value {
                    self.visit_at(value);
                    self.refuse_returned(value);
                }
                true
            }
            Stmt::TaskReturn(t) => {
                self.visit_at(&t.value);
                false
            }
            Stmt::If(s) => self.visit_if(&s.condition, &s.then_block, s.else_block.as_ref()),
            Stmt::While(s) => self.visit_loop(&LoopHead::Condition(&s.condition), &s.body, None),
            Stmt::Loop(s) => self.visit_loop(&LoopHead::None, &s.body, None),
            Stmt::For(s) => self.in_scope(|walker| {
                let head = match &s.condition {
                    Some(cond) => LoopHead::Condition(cond),
                    None => LoopHead::None,
                };
                if s.init.as_ref().is_some_and(|init| walker.visit_stmt(init)) {
                    walker.visit_unreachable(|dead| {
                        dead.visit_loop(&head, &s.body, s.update.as_ref());
                    });
                    return true;
                }
                walker.visit_loop(&head, &s.body, s.update.as_ref())
            }),
            // The loop owns what it walks (WEP: Affine Types).
            Stmt::ForOf(s) => {
                self.visit_at(&s.iterable);
                let owners = self.owners_of(&s.iterable);
                self.visit_loop(&LoopHead::Binding(&s.binding, owners), &s.body, None)
            }
            Stmt::Match(m) => self.visit_match(&m.expr, &m.arms),
            Stmt::Break(b) => {
                if let Some(value) = &b.value {
                    self.visit_at(value);
                    if let Some(label) = &b.label {
                        let owners = self.owners_of(value);
                        merge_owners(self.break_owners.entry(label.clone()).or_default(), owners);
                    }
                }
                // A jump with no target is a type error the language service
                // still walks.
                let flow = self.flow.clone();
                let target = match &b.label {
                    Some(label) => self
                        .labels
                        .iter_mut()
                        .rev()
                        .find(|(name, _)| name == label)
                        .map(|(_, end)| end),
                    None => self.loops.last_mut().map(|jumps| &mut jumps.breaks),
                };
                if let Some(target) = target {
                    reach(target, flow);
                }
                true
            }
            Stmt::Assert(a) => {
                self.visit_at(&a.condition);
                if let Some(msg) = &a.message {
                    self.visit_at(msg);
                }
                false
            }
            Stmt::LabeledBlock(lb) => self.visit_labeled_block(&lb.label, &lb.block),
            Stmt::Continue(_) => {
                let flow = self.flow.clone();
                if let Some(jumps) = self.loops.last_mut() {
                    reach(&mut jumps.continues, flow);
                }
                true
            }
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
    /// A loop whose head can end it may also run no iteration, so it ends
    /// where its head does, in the state it entered with or any an iteration
    /// left; every loop ends where each `break` lands.
    fn visit_loop(&mut self, head: &LoopHead, body: &Block, update: Option<&Expr>) -> bool {
        let entry = self.flow.clone();
        self.loops.push(LoopJumps::default());
        self.suppress += 1;
        self.visit_iteration(head, body, update);
        self.suppress -= 1;
        self.visit_iteration(head, body, update);
        let jumps = self.loops.pop().expect("pushed above");
        let end = (!head.is_endless()).then(|| {
            let mut ended = std::mem::take(&mut self.flow);
            ended.merge(entry);
            ended
        });
        self.continue_from(end, jumps.breaks)
    }

    /// The iteration's end is reached by a body that does not diverge and by
    /// each `continue`; one reached by neither leaves the next iteration the
    /// state this one began with.
    fn visit_iteration(&mut self, head: &LoopHead, body: &Block, update: Option<&Expr>) {
        let start = self.flow.clone();
        self.in_scope(|walker| {
            match head {
                LoopHead::None => {}
                LoopHead::Condition(condition) => walker.visit_condition(condition),
                LoopHead::Binding(pattern, owners) => {
                    walker.refuse_destructured_drop(pattern, None);
                    walker.bind_pattern(pattern);
                    walker.hold_bindings(pattern, owners);
                }
            }
            let diverged = walker.visit_block(body);
            let jumps = walker.loops.last_mut().expect("the loop pushed its jumps");
            let mut end = (!diverged).then(|| walker.flow.clone());
            if let Some(continued) = jumps.continues.take() {
                reach(&mut end, continued);
            }
            walker.flow = end.unwrap_or(start);
            if let Some(update) = update {
                walker.visit_at(update);
            }
        });
    }

    /// A labeled block ends where its body does and where each `break` naming
    /// it lands.
    fn visit_labeled_block(&mut self, label: &str, block: &Block) -> bool {
        self.labels.push((label.to_string(), None));
        let diverged = self.visit_block(block);
        let (_, broken) = self.labels.pop().expect("pushed above");
        let end = (!diverged).then(|| std::mem::take(&mut self.flow));
        self.continue_from(end, broken)
    }

    /// Continues from where the construct falls through, `end`, joined with
    /// where its `break`s land. Answers whether neither is reached.
    fn continue_from(&mut self, mut end: Option<Flow>, broken: Option<Flow>) -> bool {
        if let Some(broken) = broken {
            reach(&mut end, broken);
        }
        match end {
            Some(end) => {
                self.flow = end;
                false
            }
            None => true,
        }
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
        let else_div = else_block.is_some_and(|eb| self.visit_block(eb));
        match (then_div, else_div) {
            (false, false) => self.flow.merge(then_flow),
            (false, true) => self.flow = then_flow,
            (true, _) => {}
        }
        then_div && else_div
    }

    /// Note `value` as decomposed where none of `patterns` keeps it whole.
    fn note_decomposed<'p>(
        &mut self,
        value: &Expr,
        patterns: impl IntoIterator<Item = &'p Pattern>,
    ) {
        if !patterns.into_iter().any(keeps_whole) {
            self.decomposed.insert(value.id());
        }
    }

    fn visit_match(&mut self, scrutinee: &Expr, arms: &[ast::MatchArm]) -> bool {
        self.note_decomposed(scrutinee, arms.iter().map(|arm| &arm.pattern));
        self.visit_at(scrutinee);
        let owners = self.owners_of(scrutinee);
        let base = std::mem::take(&mut self.flow);
        let mut merged = base.clone();
        let mut all_diverge = true;
        for arm in arms {
            self.flow.clone_from(&base);
            let diverged = self.in_scope(|walker| {
                walker.refuse_destructured_drop(&arm.pattern, Some(scrutinee));
                walker.bind_pattern(&arm.pattern);
                walker.hold_bindings(&arm.pattern, &owners);
                if let Some(guard) = &arm.guard {
                    let saved = walker.guard_bindings.clone();
                    for_each_pattern_binding(&arm.pattern, &mut |id| {
                        walker.guard_bindings.insert(id);
                    });
                    walker.visit_at(guard);
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
            Expr::LabeledBlock(lb) => self.visit_labeled_block(&lb.label, &lb.block),
            other => {
                self.visit_at(other);
                divergence::expr_always_exits(self, other)
            }
        }
    }

    /// A condition on the path where its `let`s matched: each consumes its
    /// scrutinee and binds its pattern afresh.
    fn visit_condition(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_at(e),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    match element {
                        ConditionElement::Let { pattern, expr, .. } => {
                            self.note_decomposed(expr, [&**pattern]);
                            self.visit_at(expr);
                            self.bind_to(pattern, expr);
                        }
                        ConditionElement::Expr(e) => self.visit_at(e),
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
            Condition::Expr(e) => self.visit_at(e),
            Condition::LetChain { elements, .. } => {
                let last = elements.len() - 1;
                for (i, element) in elements.iter().enumerate() {
                    match element {
                        ConditionElement::Let { expr, .. } if i == last => {
                            self.visit(expr, OperandMode::Borrow);
                        }
                        ConditionElement::Let { .. } | ConditionElement::Expr(_) => {
                            self.visit_at(element.expr());
                        }
                    }
                }
            }
        }
    }
}

/// Whether a value `pattern` matches stays whole: bound to one name, or dropped
/// at once by `_`. A destructuring pattern takes it apart field by field.
fn keeps_whole(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Ident { .. } | Pattern::MutIdent { .. } | Pattern::Wildcard => true,
        Pattern::Typed { pattern, .. } => keeps_whole(pattern),
        Pattern::Or(alternatives) => alternatives.iter().any(keeps_whole),
        Pattern::Tuple(..)
        | Pattern::Variant { .. }
        | Pattern::Struct { .. }
        | Pattern::Literal(_)
        | Pattern::Range { .. }
        | Pattern::Error(_) => false,
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
