//! The module's inference context. A type the source leaves to inference —
//! `p.get()` in `p.get().unwrap()` — is an [`ResolvedType::InferVar`] that flows
//! on until a check [`Elaborator::equate`]s it with a type. What a variable
//! leaves undecided (a bound, a projection, a selection, a check) waits as an
//! [`Obligation`], asked again as variables settle. The sweep at each body's
//! end reports what nothing settled and substitutes the rest.

use crate::compiler_host::{CompilerHost, DiagnosticSpan};
use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{EffectRef, EffectRows, InferVarId, ResolvedType, StructDef, TypeId};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::call::assoc_target_slot;
use super::infer::unify;
use super::pending_selection::{
    ElementHome, PendingSelection, SelectedDispatch, Undecided, Waiting,
};
use super::scope::{ParamRef, ScopeParams};
use super::solver_bridge::{BoundImpls, Commitment};
use super::synth::ArgClass;
use super::trait_query::{SelfBinding, spelled_at};
use super::type_resolution::ParamSpace;
use super::types::TypeError;
use super::tysys::TypeSystem;
use crate::ast::{AssocTypeBound, AstId, AstIdSpace, GenericParam};
use crate::elaborator::sem::TypeAnnotations;
use crate::elaborator::sem::types::{
    BodyFacts, BodyFactsLens, ClosureCaptureInfo, ForOfIteratorInfo, GenericInstantiation,
    KeyValueCoercionFacts, LiteralCallee, LiteralFromCall, MethodDispatch, OperatorDispatch,
    SequenceCoercionFacts, StaticMethodDispatch,
};
use crate::elaborator::typecheck::{
    MeetRules, Meetings, NeverMeets, TypeMismatchPayload, meetings,
};
use crate::name::{FqTraitName, FqTypeName, RefKind, TypeHead, mangle_generic_name};
use crate::tir;

/// One body's inference variables and what is owed once they settle.
#[derive(Default)]
pub(crate) struct InferHoleTable {
    vars: VarStore,
    /// Whether the type being resolved is one where a written `_` is a
    /// variable.
    pub(super) holes_writable: bool,
    /// Where an error was reported since the variables last absorbed one.
    /// The error says what was wrong there, so each open variable whose
    /// origin it covers, or which its statement minted, takes the error type
    /// without a word. Recorded where the diagnostic is emitted, which reads
    /// the table, so behind a cell.
    reports: std::cell::RefCell<Vec<Report>>,
    /// The innermost statement being walked.
    pub(super) statement: Option<Span>,
    /// Whether a body is being walked, which settles everything here when it
    /// ends.
    in_body: bool,
    /// Struct literals whose field defaults were not walked, since a slot they
    /// are typed by was a hole. An unsolved hole reports itself; a solved one
    /// came too late for the defaults, so the literal reports it.
    unwalked_defaults: Vec<UnwalkedDefaults>,
    /// What is owed an answer once the variables it reads settle, in the
    /// order it was owed. Recorded where a check or a selection reads the
    /// table, so behind a cell.
    obligations: std::cell::RefCell<Vec<Obligation>>,
    /// Each bound decided of a class, so a bound required of it twice (a
    /// slot two calls reach, two variables joined) is enforced once.
    decided_bounds: IndexSet<(TypeId, FqTraitName)>,
    /// What each pending selection selected, by the id its recorded dispatch
    /// carries; `None` until it is made, and for good where it is reported.
    selections: Vec<Option<SelectedDispatch>>,
    /// The unrolled tuple `for-of` element being walked, which a selection
    /// queued now is made into the facts of.
    pub(super) element_home: Option<ElementHome>,
    /// Whether an equation is being settled, which asks the obligations once
    /// it is.
    equating: bool,
    /// The variables whose literals are about to take their default, each
    /// with the class of those literals: a bound's one impl may choose a
    /// type the class admits first.
    pub(super) defaulting_literals: Vec<(TypeId, ArgClass)>,
}

/// A question a variable left undecided. Asked again whenever a variable
/// settles ([`Elaborator::run_obligations`]): it holds and is discharged,
/// fails and is reported, or stays queued.
pub(super) enum Obligation {
    /// `var`'s answer meets a bound its slot declared. The bound is the trait
    /// its own reference site names, arguments and all, with the spelling
    /// that site wrote; the spelling alone has nothing to enforce.
    Bound {
        var: TypeId,
        param: String,
        bound: DeclaredBound,
        span: Span,
    },
    /// `ty`, which a slot not declared `affine` was given, answers no affine
    /// type once its variables settle, read in the scope that queued it.
    Copyable {
        ty: TypeId,
        is_pack: bool,
        affine_params: Vec<TypeId>,
        param: String,
        span: Span,
    },
    /// An associated-type equality of a bound on a slot a variable fills.
    Projection(ProjectionLink),
    /// An impl a call committed to while its receiver held variables. Asked
    /// only once the body ends: a reported error can be the very bound it
    /// left, so only a clean program owes the answer.
    Selection(Commitment),
    /// A type check that admitted a value while a variable on either side
    /// left it undecided.
    Check(DeferredCheck),
    /// A call that selects its impl once its receiver settles. Asked only once
    /// the body ends, by [`Elaborator::settle_pending_selections`].
    Pending(Waiting),
}

/// A type check a variable left undecided, and how its mismatch is worded.
pub(super) struct DeferredCheck {
    pub(super) actual: TypeId,
    pub(super) expected: TypeId,
    pub(super) mismatch: Box<dyn FnOnce(TypeMismatchPayload) -> TypeError>,
}

struct UnwalkedDefaults {
    holes: Vec<TypeId>,
    span: Span,
    message: String,
}

/// An associated-type equality of a bound on a slot the call deferred
/// (`S: ReflectStruct<FieldTypes = [..F]>`, `H: Holder<Item = i32>`): once the
/// owner is answered, its projection answers the right side, or is held to it.
pub(super) struct ProjectionLink {
    pub(super) owner: TypeId,
    /// A pack's bound holds of each element of the tuple the pack answers.
    pub(super) each_element: bool,
    /// The right side at the call: the slot it names, or the type it spells.
    pub(super) target: TypeId,
    pub(super) trait_: DefId,
    /// The bound's own arguments as the call read them, `None` where it
    /// writes none.
    pub(super) written: Option<Vec<TypeId>>,
    pub(super) assoc: AssocTypeBound,
    pub(super) trait_name: String,
    pub(super) span: Span,
}

impl InferHoleTable {
    pub(super) fn note_report(&self, span: DiagnosticSpan) {
        self.reports.borrow_mut().push(Report {
            span,
            statement: self.statement,
        });
    }

    pub(super) fn require(&self, obligation: Obligation) {
        self.obligations.borrow_mut().push(obligation);
    }

    /// Queue `selection`, to be made into the facts of the walk queuing it.
    pub(super) fn require_pending(&self, selection: PendingSelection) {
        self.require(Obligation::Pending(Waiting {
            selection,
            home: self.element_home,
        }));
    }

    /// The id of a selection yet to be made.
    pub(super) fn reserve_selection(&mut self) -> usize {
        self.selections.push(None);
        self.selections.len() - 1
    }

    pub(super) fn select(&mut self, id: usize, selected: SelectedDispatch) {
        assert!(self.selections[id].is_none(), "a selection is made once");
        self.selections[id] = Some(selected);
    }

    /// Take the pending selections out of the queue, in the order they were
    /// owed.
    pub(super) fn take_pending(&self) -> Vec<Waiting> {
        let mut queue = self.obligations.borrow_mut();
        let (pending, rest): (Vec<Obligation>, Vec<Obligation>) = std::mem::take(&mut *queue)
            .into_iter()
            .partition(|o| matches!(o, Obligation::Pending(_)));
        *queue = rest;
        pending
            .into_iter()
            .map(|o| match o {
                Obligation::Pending(waiting) => waiting,
                Obligation::Bound { .. }
                | Obligation::Copyable { .. }
                | Obligation::Projection(_)
                | Obligation::Selection(_)
                | Obligation::Check(_) => unreachable!("partitioned to the pending ones"),
            })
            .collect()
    }
}

/// An error reported, and the statement it was reported in.
struct Report {
    span: DiagnosticSpan,
    statement: Option<Span>,
}

impl Report {
    /// Whether it covers the origin's span, or points into the statement the
    /// origin was minted in. A report raised there that points elsewhere,
    /// such as into a callee's default argument, is about that code instead.
    /// A body with no statement, such as a global's initializer, is one.
    fn covers(&self, origin: &Origin) -> bool {
        let reported = Extent::of_report(&self.span);
        let in_statement = self.statement == origin.statement
            && self
                .statement
                .is_none_or(|statement| Extent::of(&statement).contains(&reported));
        in_statement
            || origin
                .span()
                .is_some_and(|span| reported.contains(&Extent::of(&span)))
    }
}

/// A source range in one parse.
struct Extent {
    space: AstIdSpace,
    start: (usize, usize),
    end: (usize, usize),
}

impl Extent {
    fn of(span: &Span) -> Self {
        Extent {
            space: span.space,
            start: (span.line, span.column),
            end: (span.end_line, span.end_column),
        }
    }

    fn of_report(span: &DiagnosticSpan) -> Self {
        Extent {
            space: span.space,
            start: (span.line, span.column),
            end: (
                span.end_line.unwrap_or(span.line),
                span.end_column.unwrap_or(span.column),
            ),
        }
    }

    fn contains(&self, inner: &Extent) -> bool {
        self.space == inner.space && self.start <= inner.start && inner.end <= self.end
    }
}

/// A trait bound a slot declared: the trait its reference site names, with the
/// arguments it writes, and the spelling that site wrote.
#[derive(Clone, Debug)]
pub(crate) struct DeclaredBound {
    pub(crate) trait_: FqTraitName,
    pub(crate) written: String,
    /// A pack's bound (`..F: Make`), which binds each element of the tuple
    /// the pack is answered with rather than the tuple.
    pub(crate) each_element: bool,
}

/// Which of a declaration's bounds a call asks, by what it knows of the
/// declaration's own effect parameters: only the call's arguments say what
/// one stands for.
#[derive(Clone, Copy)]
pub(crate) enum BoundRows<'r> {
    /// Before the arguments: a bound naming an effect parameter is left out,
    /// for the call to ask once they are known.
    Unbound,
    /// Every bound, at what the call binds the parameters to.
    All(&'r EffectRows),
    /// Only the bounds [`Self::Unbound`] left out.
    Left(&'r EffectRows),
}

/// Whether a row in `ty` names an effect parameter.
fn names_effect_param(ty: &FqTypeName) -> bool {
    ty.mentions(&|head| {
        matches!(head, TypeHead::Function { effects, .. } if effects.iter().any(EffectRef::is_param))
    })
}

/// What an unsolved variable reports.
///
/// `owner` is both the tail of the sentence and the coalescing key: the
/// variables one use site minted share it, so several unsolved slots of one
/// call name themselves in a single message rather than one apiece.
struct Blame {
    span: Span,
    /// The parameter this variable stands for, when a declaration names it.
    param: Option<String>,
    owner: String,
}

impl Blame {
    /// The sentence, with `params` in place of this blame's own parameter.
    fn message(&self, params: &[&str]) -> String {
        if params.is_empty() {
            return self.owner.clone();
        }
        TypeError::cannot_infer_naming(params, &self.owner)
    }
}

impl InferHoleTable {
    pub(super) fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.obligations.borrow().is_empty()
    }
}

/// The body's inference variables, as a union-find: a variable answered by
/// another follows it, and only a root holds a binding.
#[derive(Default)]
struct VarStore {
    /// Each variable's type, in mint order.
    holes: Vec<TypeId>,
    index: IndexMap<TypeId, usize>,
    parent: Vec<usize>,
    /// A root's answer, `None` while it has none.
    binding: Vec<Option<TypeId>>,
    /// Each variable's origin, in mint order.
    origins: Vec<Origin>,
    /// How many answers the store has taken; each leaves one class fewer open.
    answers: usize,
}

/// Where a variable came from, which is what it reports if left open.
#[derive(Default)]
struct Origin {
    /// The slot it stands for, once a use site names one.
    blame: Option<Blame>,
    /// Where the source wrote the `_` it is, while that `_` reports itself.
    written: Option<Span>,
    /// The statement it was minted in.
    statement: Option<Span>,
}

impl Origin {
    /// Where a report about this variable would point.
    fn span(&self) -> Option<Span> {
        self.blame.as_ref().map(|b| b.span).or(self.written)
    }
}

impl VarStore {
    fn is_empty(&self) -> bool {
        self.holes.is_empty()
    }

    fn len(&self) -> usize {
        self.holes.len()
    }

    fn mint(&mut self, hole: TypeId, statement: Option<Span>) {
        let i = self.holes.len();
        assert!(
            self.index.insert(hole, i).is_none(),
            "inference variable {hole:?} minted twice"
        );
        self.holes.push(hole);
        self.parent.push(i);
        self.binding.push(None);
        self.origins.push(Origin {
            statement,
            ..Origin::default()
        });
    }

    fn contains(&self, hole: TypeId) -> bool {
        self.index.contains_key(&hole)
    }

    fn origin_mut(&mut self, hole: TypeId) -> &mut Origin {
        &mut self.origins[self.index[&hole]]
    }

    /// `blame` as the one report `hole`'s class makes, in place of any its
    /// members were minted with.
    fn reword_class(&mut self, hole: TypeId, blame: Blame) {
        let root = self.root(self.index[&hole]);
        for i in 0..self.len() {
            if self.root(i) == root {
                self.origins[i].blame = None;
            }
        }
        self.origin_mut(hole).blame = Some(blame);
    }

    /// Each variable with its origin, in mint order.
    fn with_origins(&self) -> impl Iterator<Item = (TypeId, &Origin)> {
        self.holes.iter().copied().zip(&self.origins)
    }

    /// The classes a variable blamed for a slot joined, in mint order.
    fn blamed_classes(&self) -> IndexSet<TypeId> {
        (0..self.len())
            .filter(|&i| self.origins[i].blame.is_some())
            .map(|i| self.holes[self.root(i)])
            .collect()
    }

    fn root(&self, mut i: usize) -> usize {
        while self.parent[i] != i {
            i = self.parent[i];
        }
        i
    }

    /// The variable standing for `hole`'s class, `None` for a variable this
    /// module did not mint.
    fn root_of(&self, hole: TypeId) -> Option<TypeId> {
        let &i = self.index.get(&hole)?;
        Some(self.holes[self.root(i)])
    }

    /// Whether `hole`'s class is not bound to a type. A variable this module
    /// did not mint has no class to bind.
    fn is_unanswered(&self, hole: TypeId) -> bool {
        self.index
            .get(&hole)
            .is_some_and(|&i| self.binding[self.root(i)].is_none())
    }

    /// What `hole` answers to one step on: the variable standing for its
    /// class, or the class's binding.
    fn answer(&self, hole: TypeId) -> Option<TypeId> {
        let &i = self.index.get(&hole)?;
        if self.parent[i] != i {
            return Some(self.holes[self.root(i)]);
        }
        self.binding[i]
    }

    /// Answer `hole`'s unbound class with `answer`: it joins `answer`'s class
    /// where that is another of the module's variables, and is bound to it
    /// otherwise.
    fn answer_with(&mut self, hole: TypeId, answer: TypeId) {
        assert!(self.is_unanswered(hole), "a class is bound once");
        let i = self.root(self.index[&hole]);
        match self.index.get(&answer) {
            Some(&j) => {
                let root = self.root(j);
                assert_ne!(root, i, "the occurs check keeps a variable off itself");
                self.parent[i] = root;
            }
            None => self.binding[i] = Some(answer),
        }
        self.answers += 1;
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// A variable for a call's type argument `slot` nothing answered yet,
    /// reported at the end of the body as `param_name` [`owner`](
    /// TypeError::cannot_infer_owner) if nothing does. A variable inference
    /// already put there takes the blame and the bounds: a second one would
    /// orphan it, which the sweep would then pin to `error` behind a diagnostic
    /// nobody asked for.
    pub(super) fn defer_slot(
        &mut self,
        slot: Option<TypeId>,
        span: Span,
        owner: String,
        param_name: String,
        bounds: Vec<DeclaredBound>,
    ) -> TypeId {
        let blame = Blame {
            span,
            param: Some(param_name.clone()),
            owner,
        };
        let existing = slot.filter(|&t| self.tysys.type_table.borrow().contains_infer_var(t));
        let var = match existing {
            Some(var) => var,
            None => self.mint_infer_var_named(&param_name),
        };
        // The site words the report: the blame the variable was minted with
        // knows only the declaration.
        self.infer_holes.vars.reword_class(var, blame);
        self.attach_infer_var_bounds(var, param_name, bounds, span);
        var
    }

    /// The bounds `param` declares, each as the trait its own site names plus
    /// the spelling that site wrote, with the enclosing impl's parameters at
    /// `impl_args`, which the solution is checked against long after the
    /// receiver that settled them is gone.
    ///
    /// A bound whose site reaches no declaration is dropped: it is diagnosed
    /// where it was written, and there is nothing to enforce a solution
    /// against.
    ///
    /// `rows` says which bounds a call asks, by what it knows of the
    /// declaration's own effect parameters.
    pub(super) fn declared_bounds(
        &mut self,
        param: &GenericParam,
        self_binding: Option<SelfBinding>,
        impl_args: &[(ParamRef, TypeId)],
        rows: BoundRows<'_>,
    ) -> Vec<DeclaredBound> {
        // A variable stays named, for the re-check to read its answer; a
        // parameter is the frame's, and only that frame spells it.
        let at_impl: Vec<(FqTypeName, FqTypeName)> = {
            let tt = self.tysys.type_table.borrow();
            let resolutions = &self.tysys.resolutions;
            impl_args
                .iter()
                .filter(|&&(_, arg)| !tt.contains_rigid_param(arg) && !tt.contains_unknown(arg))
                .map(|(param, arg)| (param.binder(resolutions), tt.fq_type_name(*arg)))
                .collect()
        };
        param
            .real_bounds()
            .into_iter()
            .filter_map(|b| {
                let trait_ = self.tysys.bound_written(&b)?;
                let trait_ = self.bound_trait_at_args(trait_, &b, &[], &[], self_binding);
                let names = trait_.args().iter().any(names_effect_param);
                let bound_at = match rows {
                    BoundRows::Unbound if names => return None,
                    BoundRows::Unbound => None,
                    BoundRows::Left(_) if !names => return None,
                    BoundRows::All(at) | BoundRows::Left(at) => Some(at),
                };
                let trait_ = match bound_at {
                    Some(at) => TypeTable::trait_at_rows(&trait_, at),
                    None => trait_,
                };
                Some(DeclaredBound {
                    trait_: spelled_at(&trait_, &at_impl),
                    written: b.name,
                    each_element: param.is_pack,
                })
            })
            .collect()
    }

    /// Mint a fresh inference variable carrying no diagnostic yet.
    ///
    /// A use site attaches the diagnostic only to the variables it commits
    /// to, so one nobody kept reports nothing.
    pub(super) fn mint_infer_var(&mut self) -> TypeId {
        self.mint_infer_var_for(None)
    }

    /// [`Self::mint_infer_var`] for a named slot, which a diagnostic renders
    /// the variable as.
    pub(super) fn mint_infer_var_named(&mut self, slot_name: &str) -> TypeId {
        self.mint_infer_var_for(Some(slot_name))
    }

    fn mint_infer_var_for(&mut self, slot_name: Option<&str>) -> TypeId {
        let var = InferVarId(self.infer_holes.vars.len() as u32);
        let hole = {
            let mut tt = self.tysys.type_table.borrow_mut();
            let hole = tt.make_infer_var(var);
            tt.set_infer_var_name(var, slot_name.map(str::to_string));
            hole
        };
        let statement = self.infer_holes.statement;
        self.infer_holes.vars.mint(hole, statement);
        hole
    }

    /// A variable for the `_` written at `span`, fresh for each elaboration
    /// of it: an unrolled tuple `for-of` and a default argument elaborate one
    /// `_` many times, each a use of its own.
    pub(super) fn written_hole(&mut self, span: Span) -> TypeId {
        let var = self.mint_infer_var_named("_");
        self.infer_holes.vars.origin_mut(var).written = Some(span);
        var
    }

    /// Whether `ty` holds a variable of `var`'s class.
    pub(super) fn holds_class_of(&self, ty: TypeId, var: TypeId) -> bool {
        let vars = &self.infer_holes.vars;
        let Some(root) = vars.root_of(var) else {
            return false;
        };
        let ty = self.apply_infer_holes(ty);
        let held = self.tysys.type_table.borrow().infer_vars_in(ty);
        held.into_iter().any(|v| vars.root_of(v) == Some(root))
    }

    /// Whether a queued associated-type equality answers `var`, wherever its
    /// target holds it (`Item = List<B>`), once its owner is answered. An
    /// owner holding `var` itself (`A: Src<This = A>`) answers nothing new.
    pub(super) fn projection_answers(&self, var: TypeId) -> bool {
        self.infer_holes
            .obligations
            .borrow()
            .iter()
            .any(|o| match o {
                Obligation::Projection(link) => {
                    self.holds_class_of(link.target, var) && !self.holds_class_of(link.owner, var)
                }
                Obligation::Bound { .. }
                | Obligation::Copyable { .. }
                | Obligation::Selection(_)
                | Obligation::Check(_)
                | Obligation::Pending(_) => false,
            })
    }

    /// Whether `ty` is a `_` the source wrote for a whole slot, still open.
    pub(super) fn is_bare_written_hole(&self, ty: TypeId) -> bool {
        let applied = self.apply_infer_holes(ty);
        self.tysys.type_table.borrow().is_infer_var(applied) && self.holds_written_hole(ty)
    }

    /// `args` with each `_` standing for a whole slot made `unknown`, an
    /// omitted slot, which the site's own inference answers and reports.
    pub(super) fn omit_bare_written_holes(&mut self, args: Vec<TypeId>) -> Vec<TypeId> {
        args.into_iter()
            .map(|arg| {
                if !self.is_bare_written_hole(arg) {
                    return arg;
                }
                self.unwrite_holes_in(arg);
                TypeTable::UNKNOWN
            })
            .collect()
    }

    /// Blame each `_` `arg` holds with `message`, the sentence for the slot
    /// `param` it fills. One for the whole slot is replaced by a variable named
    /// for `param` and blamed alike, joined with it.
    pub(super) fn blame_in_slot(
        &mut self,
        arg: &mut TypeId,
        span: Span,
        message: &str,
        param: String,
    ) {
        if self.is_bare_written_hole(*arg) {
            let blamed = self.mint_infer_var_named(&param);
            self.attach_infer_var_diag(blamed, span, message.to_string());
            self.equate(*arg, blamed);
            self.unwrite_holes_in(*arg);
            *arg = blamed;
            return;
        }
        for hole in self.written_holes_in(*arg) {
            self.attach_infer_var_diag(hole, span, message.to_string());
        }
    }

    /// Let the `_`s `ty` holds go unreported: a site that resolves what they
    /// were written in again answers and reports its own.
    pub(super) fn unwrite_holes_in(&mut self, ty: TypeId) {
        let held = self.tysys.type_table.borrow().infer_vars_in(ty);
        for hole in held {
            if self.infer_holes.vars.contains(hole) {
                self.infer_holes.vars.origin_mut(hole).written = None;
            }
        }
    }

    /// The variables `tys` name whose class holds an open `_` the source wrote.
    pub(super) fn vars_holding_written_holes(&self, tys: &[TypeId]) -> IndexSet<TypeId> {
        tys.iter()
            .flat_map(|&ty| self.tysys.type_table.borrow().infer_vars_in(ty))
            .filter(|&var| self.holds_written_hole(var))
            .collect()
    }

    /// Whether `ty` still holds an open `_` the source wrote, at any depth.
    pub(super) fn holds_written_hole(&self, ty: TypeId) -> bool {
        !self.written_holes_in(ty).is_empty()
    }

    /// The variables of the `_`s the source wrote that `ty` still holds.
    pub(super) fn written_holes_in(&self, ty: TypeId) -> Vec<TypeId> {
        let ty = self.apply_infer_holes(ty);
        let vars = &self.infer_holes.vars;
        let held: IndexSet<Option<TypeId>> = self
            .tysys
            .type_table
            .borrow()
            .infer_vars_in(ty)
            .into_iter()
            .map(|h| vars.root_of(h))
            .collect();
        vars.with_origins()
            .filter(|(w, origin)| {
                origin.written.is_some()
                    && vars.is_unanswered(*w)
                    && held.contains(&vars.root_of(*w))
            })
            .map(|(w, _)| w)
            .collect()
    }

    /// Answer each `_` `written` holds with what `expected` has in its place.
    /// A place `expected` does not reach, such as one a coercion would bridge,
    /// stays open.
    pub(super) fn settle_written_holes_from(&mut self, written: TypeId, expected: TypeId) {
        let holes = self.written_holes_in(written);
        if holes.is_empty() {
            return;
        }
        let written = self.apply_infer_holes(written);
        let mut bindings = IndexMap::default();
        unify(&self.tysys.type_table, written, expected, &mut bindings);
        for (var, answer) in bindings {
            if holes.contains(&var) {
                self.settle_as_written(var, answer);
            }
        }
    }

    /// Require of `var`'s answer the trait bounds its slot declared.
    pub(super) fn attach_infer_var_bounds(
        &mut self,
        var: TypeId,
        param_name: String,
        bounds: Vec<DeclaredBound>,
        span: Span,
    ) {
        if bounds.is_empty() {
            return;
        }
        for bound in bounds {
            self.infer_holes.require(Obligation::Bound {
                var,
                param: param_name.clone(),
                bound,
                span,
            });
        }
        self.run_obligations();
    }

    /// Check `param`'s bounds against what `type_arg`, which still names
    /// variables, becomes once they are solved: at finalize, through a fresh
    /// variable chained to it. `type_arg`'s own variables keep the bounds of
    /// the slots they stand for.
    pub(super) fn defer_bounds_to_answer(
        &mut self,
        param: &GenericParam,
        type_arg: TypeId,
        self_binding: Option<SelfBinding>,
        impl_args: &[(ParamRef, TypeId)],
        rows: BoundRows<'_>,
        span: Span,
    ) {
        let bounds = self.declared_bounds(param, self_binding, impl_args, rows);
        self.defer_declared_bounds(param, type_arg, bounds, span);
    }

    /// [`Self::defer_bounds_to_answer`] for `bounds` already declared. A
    /// settled `type_arg` defers them too where their own arguments name
    /// variables still to be answered (`T: Pick<U>` with `U` awaiting `1.5`).
    pub(super) fn defer_declared_bounds(
        &mut self,
        param: &GenericParam,
        type_arg: TypeId,
        bounds: Vec<DeclaredBound>,
        span: Span,
    ) {
        if bounds.is_empty() {
            return;
        }
        let witness = self.mint_infer_var();
        self.equate(witness, type_arg);
        self.attach_infer_var_bounds(witness, param.name.clone(), bounds, span);
    }

    /// Set the diagnostic `var` raises if it is never solved. The first one
    /// wins.
    pub(super) fn attach_infer_var_diag(&mut self, var: TypeId, span: Span, message: String) {
        self.blame_once(
            var,
            Blame {
                span,
                param: None,
                owner: message,
            },
        );
    }

    /// Leave `var` unreported where nothing answered it: the site settled its
    /// slot otherwise, or reports it as it hands it on.
    pub(super) fn waive_open(&mut self, var: TypeId) {
        if self.infer_holes.vars.is_unanswered(var) {
            self.infer_holes.vars.origin_mut(var).blame = None;
        }
    }

    fn blame_once(&mut self, var: TypeId, blame: Blame) {
        self.infer_holes
            .vars
            .origin_mut(var)
            .blame
            .get_or_insert(blame);
    }

    /// Leave a struct literal's field defaults unwalked while `holes` are
    /// open, and report the literal with `message` if they are all solved.
    pub(super) fn leave_defaults_unwalked(
        &mut self,
        holes: Vec<TypeId>,
        span: Span,
        message: String,
    ) {
        assert!(
            holes
                .iter()
                .all(|&hole| self.infer_holes.vars.contains(hole)),
            "a struct literal's open slot is a hole finalize settles"
        );
        self.infer_holes.unwalked_defaults.push(UnwalkedDefaults {
            holes,
            span,
            message,
        });
    }

    /// Whether every variable `ty` names is still unanswered and reports
    /// itself if it stays so.
    pub(super) fn awaits_report(&self, tt: &TypeTable, ty: TypeId) -> bool {
        let store = &self.infer_holes.vars;
        let vars = tt.infer_vars_in(ty);
        let blamed = store.blamed_classes();
        !vars.is_empty()
            && vars.iter().all(|&var| {
                store.is_unanswered(var)
                    && store
                        .root_of(var)
                        .is_some_and(|class| blamed.contains(&class))
            })
    }

    /// Blame each open `_` `arg` holds as the slot `param` it fills would be.
    pub(super) fn blame_holes_as_slot(
        &mut self,
        arg: TypeId,
        span: Span,
        param: &str,
        owner: &str,
    ) {
        for hole in self.written_holes_in(arg) {
            self.attach_infer_var_blame(hole, span, param.to_string(), owner.to_string());
        }
    }

    /// [`Self::attach_infer_var_diag`] for a variable standing in for a named
    /// parameter, so unsolved siblings of one use site coalesce.
    pub(super) fn attach_infer_var_blame(
        &mut self,
        var: TypeId,
        span: Span,
        param: String,
        owner: String,
    ) {
        self.blame_once(
            var,
            Blame {
                span,
                param: Some(param),
                owner,
            },
        );
    }

    /// Whether the type argument `t` is a slot nothing answers: a bare
    /// parameter or variable, neither one of the enclosing declaration's
    /// `scope_params` nor a variable an enclosing call's pending literals
    /// answer.
    pub(super) fn slot_unanswered(&self, t: TypeId, scope_params: &ScopeParams) -> bool {
        self.is_open_slot(t, scope_params) && !self.awaits_pending_call(t)
    }

    /// Whether a declared default fills the type argument `t`: a slot nothing
    /// answers, or one that is an enclosing call's open variable, which the
    /// default then answers in turn (`ident(it.collect())`). A variable an
    /// enclosing call waits at for its literals keeps waiting: one chained to
    /// that call's, or one it took over (`same(wrapd(1), x)`).
    pub(super) fn slot_takes_default(&self, t: TypeId, scope_params: &ScopeParams) -> bool {
        let chained = self.apply_infer_holes(t) != t && self.awaits_pending_call(t);
        self.is_open_slot(t, scope_params) && !chained && !self.annotate_ctx.literals_wait_at(t)
    }

    /// Whether `t` is a bare parameter or variable that is not one of the
    /// enclosing declaration's `scope_params`.
    pub(super) fn is_open_slot(&self, t: TypeId, scope_params: &ScopeParams) -> bool {
        let t = self.apply_infer_holes(t);
        self.tysys.is_unbound_type_param(t)
            && !scope_params.owns(&self.tysys.type_table.borrow(), t)
    }

    pub(super) fn type_has_infer_hole(&self, ty: TypeId) -> bool {
        if self.infer_holes.is_empty() {
            return false;
        }
        self.tysys.type_table.borrow().contains_infer_var(ty)
    }

    /// Whether the tuple `ty` holds a pack variable still open, reported and
    /// closed: `reading` needs each element's position, which only the
    /// variable's answer gives.
    pub(super) fn reports_open_pack(&mut self, ty: TypeId, reading: &str, span: Span) -> bool {
        let ty = self.apply_infer_holes(ty);
        let open = {
            let table = self.tysys.type_table.borrow();
            table.as_tuple_through_ref(ty).and_then(|(elems, _)| {
                elems
                    .into_iter()
                    .find(|&elem| matches!(table.get(elem), ResolvedType::PackVar { .. }))
                    .map(|pack| (table.name(pack), table.infer_vars_in(pack)))
            })
        };
        let Some((name, vars)) = open else {
            return false;
        };
        let _ = self.emit(TypeError::CannotInferType {
            message: format!("cannot infer `{name}` far enough to {reading}; annotate it"),
            span,
        });
        for var in vars {
            self.close_infer_var(var);
        }
        true
    }

    /// Pin `var` to the error type: a reported diagnostic already covers it,
    /// and nothing later may settle it.
    pub(super) fn close_infer_var(&mut self, var: TypeId) {
        self.answer_infer_var(var, TypeTable::ERROR);
    }

    /// Settle with the error type each open variable an error reported since
    /// the last call covers: that error is the whole fault, so neither the
    /// variable nor its class asks for itself as well.
    fn absorb_reports(&mut self) {
        let reports = self.infer_holes.reports.take();
        if reports.is_empty() {
            return;
        }
        let vars = &self.infer_holes.vars;
        let covered: Vec<TypeId> = vars
            .with_origins()
            .filter(|(hole, origin)| {
                vars.is_unanswered(*hole) && reports.iter().any(|report| report.covers(origin))
            })
            .map(|(hole, _)| hole)
            .collect();
        for hole in covered {
            self.close_infer_var(hole);
        }
    }

    /// [`Self::equate`] `var` with a type the source wrote for it. A written
    /// `!` answers it, where an inferred one, a diverging arm's, says nothing.
    pub(super) fn settle_as_written(&mut self, var: TypeId, written: TypeId) {
        if written == TypeTable::NEVER {
            self.answer_infer_var(var, written);
        } else {
            self.equate(var, written);
        }
    }

    /// Record `answer` for `var` unless it has one, which every other answer
    /// goes through: a target its owner's projection answers is answered
    /// with it here, before anything reads the target.
    ///
    /// Refused where `answer` names `var`'s own class: one of its variables is
    /// already `var`, and a type holding one is no type at all.
    fn answer_infer_var(&mut self, var: TypeId, answer: TypeId) -> bool {
        if !self.infer_holes.vars.is_unanswered(var) {
            return false;
        }
        let answer = self.apply_infer_holes(answer);
        if self.holds_class_of(answer, var) {
            return false;
        }
        self.infer_holes.vars.answer_with(var, answer);
        if !self.infer_holes.equating {
            self.run_obligations();
        }
        true
    }

    /// Make a value's type `actual` one with its slot's `expected` wherever a
    /// variable on either side is what leaves the check undecided: the
    /// variable is bound to the other side, or joins it where that is a
    /// variable too. A side naming a callee's own parameter binds nothing,
    /// since the callee's solve answers it.
    pub(super) fn equate(&mut self, actual: TypeId, expected: TypeId) {
        self.equate_meeting(actual, expected, NeverMeets::Nothing);
    }

    /// [`Self::equate`], where a `!` in `actual` also answers the variable it
    /// meets. A value of `!` fits any type, so a call reads it only once
    /// everything else has had its say.
    pub(super) fn equate_with_never(&mut self, actual: TypeId, expected: TypeId) {
        self.equate_meeting(actual, expected, NeverMeets::Variables);
    }

    /// Settle every pair the two types meet at, then ask the obligations once:
    /// an equation holds as a whole, so nothing it settles is read by an
    /// obligation before the rest of it is.
    fn equate_meeting(&mut self, actual: TypeId, expected: TypeId, never: NeverMeets) {
        if !self.type_has_infer_hole(actual) && !self.type_has_infer_hole(expected) {
            return;
        }
        self.absorb_reports();
        let scope = self.scope_type_param_ids();
        let [actual_bases, expected_bases] = [actual, expected].map(|written| {
            let tt = self.tysys.type_table.borrow();
            tt.assoc_type_projections(written)
                .into_iter()
                .map(|projection| {
                    let ResolvedType::AssocTypeProjection { param_id, .. } = tt.get(projection)
                    else {
                        unreachable!("`assoc_type_projections` yields projections")
                    };
                    *param_id
                })
                .collect::<Vec<TypeId>>()
        });
        let outer = std::mem::replace(&mut self.infer_holes.equating, true);
        loop {
            let mut met = Meetings::default();
            meetings(
                self.apply_infer_holes(actual),
                self.apply_infer_holes(expected),
                &self.tysys.type_table.borrow(),
                MeetRules {
                    never,
                    written_base: &|base| {
                        actual_bases.contains(&base) && expected_bases.contains(&base)
                    },
                },
                &mut met,
            );
            let runs: Vec<(TypeId, TypeId)> = met
                .packs
                .into_iter()
                .map(|run| {
                    let tuple = self.tysys.type_table.borrow_mut().make_tuple(run.elems);
                    (tuple, run.var)
                })
                .collect();
            if !met
                .pairs
                .into_iter()
                .chain(runs)
                .any(|(a, e)| self.settle_meeting(a, e, &scope))
            {
                break;
            }
        }
        self.infer_holes.equating = outer;
        if !outer {
            self.run_obligations();
        }
    }

    /// Bind or join the unanswered variable of `a` and `e`; whether it did.
    fn settle_meeting(&mut self, a: TypeId, e: TypeId, scope: &ScopeParams) -> bool {
        let vars = &self.infer_holes.vars;
        let (var, other) = if vars.is_unanswered(a) {
            (a, e)
        } else if vars.is_unanswered(e) {
            (e, a)
        } else {
            return false;
        };
        let tt = self.tysys.type_table.borrow();
        if names_foreign_param(&tt, other, scope) {
            return false;
        }
        // The variable an enclosing call's literals answer stays the one
        // they read.
        let joins_waiting = tt.is_infer_var(other) && self.annotate_ctx.literals_wait_at(var);
        drop(tt);
        if joins_waiting {
            self.answer_infer_var(other, var)
        } else {
            self.answer_infer_var(var, other)
        }
    }

    /// Pin a hole the argument carried in (`let v = gen()?; foo(v)`) against
    /// its parameter type.
    pub(super) fn pin_arg_hole_against(&mut self, arg: &mut TypeId, expected: TypeId) {
        if self.type_has_infer_hole(*arg) {
            self.equate(*arg, expected);
            *arg = self.apply_infer_holes(*arg);
        }
    }

    /// Substitute solved holes into `ty` now — used before a site that records a
    /// mangled name derived from the type, which a later sweep could not fix.
    pub(super) fn apply_infer_holes(&self, ty: TypeId) -> TypeId {
        if !self.type_has_infer_hole(ty) {
            return ty;
        }
        let mut subst = IndexMap::default();
        self.reached_answers(ty, &mut subst);
        if subst.is_empty() {
            return ty;
        }
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_infer_vars(ty, &subst)
    }

    /// [`Self::apply_infer_holes`] on each of `tys`.
    pub(super) fn apply_infer_holes_each(&self, tys: &[TypeId]) -> Vec<TypeId> {
        tys.iter().map(|&ty| self.apply_infer_holes(ty)).collect()
    }

    /// Add to `subst` the final answer of each solved variable `ty` reaches,
    /// following each class to its binding.
    fn reached_answers(&self, ty: TypeId, subst: &mut IndexMap<InferVarId, TypeId>) {
        let reached = self.tysys.type_table.borrow().infer_vars_in(ty);
        for hole in reached {
            let ResolvedType::InferVar(var) = *self.tysys.type_table.borrow().get(hole) else {
                panic!("infer_vars_in answers variables");
            };
            if subst.contains_key(&var) {
                continue;
            }
            let Some(answer) = self.infer_holes.vars.answer(hole) else {
                continue;
            };
            // The occurs check keeps chains acyclic, so this recursion ends.
            self.reached_answers(answer, subst);
            let answer = self
                .tysys
                .type_table
                .borrow_mut()
                .substitute_infer_vars(answer, subst);
            subst.insert(var, answer);
        }
    }

    /// The final `variable → replacement` map over every variable, an
    /// unsolved one standing for `error` so nothing leaks past finalize.
    fn pinned_hole_subst(&self) -> IndexMap<InferVarId, TypeId> {
        let holes: Vec<TypeId> = self.infer_holes.vars.holes.clone();
        let mut subst = IndexMap::default();
        for &hole in &holes {
            self.reached_answers(hole, &mut subst);
        }
        let mut tt = self.tysys.type_table.borrow_mut();
        for &hole in &holes {
            let ResolvedType::InferVar(var) = *tt.get(hole) else {
                panic!("infer-hole table holds a non-variable type");
            };
            subst.entry(var).or_insert(TypeTable::ERROR);
        }
        // An unsolved variable an answer still names is now `error`, which
        // names nothing, so one pass ends every chain.
        for i in 0..subst.len() {
            subst[i] = tt.substitute_infer_vars(subst[i], &subst);
        }
        subst
    }

    /// Report each variable `blames` names as never answered. One use site's
    /// slots, in mint order, become one message naming them all.
    fn report_uninferred(&self, blames: Vec<Blame>) {
        let mut groups: IndexMap<(Span, String), Vec<Option<String>>> = IndexMap::default();
        let mut owners: IndexMap<(Span, String), Blame> = IndexMap::default();
        for blame in blames {
            let key = (blame.span, blame.owner.clone());
            groups
                .entry(key.clone())
                .or_default()
                .push(blame.param.clone());
            owners.entry(key).or_insert(blame);
        }
        for (key, params) in groups {
            let named: Vec<&str> = params.iter().filter_map(Option::as_deref).collect();
            let mut seen: IndexSet<&str> = IndexSet::default();
            let unique: Vec<&str> = named.into_iter().filter(|p| seen.insert(p)).collect();
            let blame = &owners[&key];
            let _ = self.emit(TypeError::CannotInferType {
                message: blame.message(&unique),
                span: blame.span,
            });
        }
    }

    /// Take the blames of `classes`, each by the earliest variable joined
    /// into it that carries one, so a class reports once.
    fn class_blames(&mut self, classes: &IndexSet<TypeId>) -> Vec<Blame> {
        let vars = &mut self.infer_holes.vars;
        let mut reported: IndexSet<TypeId> = IndexSet::default();
        let mut blames = Vec::new();
        for i in 0..vars.len() {
            let root = vars.holes[vars.root(i)];
            if !classes.contains(&root) {
                continue;
            }
            if let Some(blame) = vars.origins[i].blame.take()
                && reported.insert(root)
            {
                blames.push(blame);
            }
        }
        blames
    }

    /// Report `vars` as never answered now, then pin each to the error type so
    /// nothing later settles them. A class carrying no blame closes unreported,
    /// as the end of the body closes it: what left it open reports there, a
    /// selection still waiting among them.
    pub(super) fn close_uninferred(&mut self, vars: &[TypeId]) {
        self.absorb_reports();
        let vars: Vec<TypeId> = vars
            .iter()
            .copied()
            .filter(|&var| self.infer_holes.vars.is_unanswered(var))
            .collect();
        let classes: IndexSet<TypeId> = vars
            .iter()
            .filter_map(|&var| self.infer_holes.vars.root_of(var))
            .collect();
        self.blame_bare_written_holes(|root| classes.contains(&root));
        let blames = self.class_blames(&classes);
        self.report_uninferred(blames);
        for var in vars {
            self.close_infer_var(var);
        }
    }

    /// [`Self::close_uninferred`] on the variables still open in `ty`, and
    /// whether there was one.
    pub(super) fn closes_open_vars(&mut self, ty: TypeId) -> bool {
        let open = self.tysys.type_table.borrow().infer_vars_in(ty);
        self.close_uninferred(&open);
        !open.is_empty()
    }

    /// Blame each open `_` the source wrote in a class `within` takes and no
    /// slot blames, at the `_`.
    fn blame_bare_written_holes(&mut self, within: impl Fn(TypeId) -> bool) {
        let vars = &self.infer_holes.vars;
        let blamed = vars.blamed_classes();
        let bare: Vec<(Span, TypeId)> = vars
            .with_origins()
            .filter_map(|(w, origin)| Some((origin.written?, w)))
            .filter(|&(_, w)| {
                vars.is_unanswered(w)
                    && vars
                        .root_of(w)
                        .is_some_and(|root| !blamed.contains(&root) && within(root))
            })
            .collect();
        for (span, hole) in bare {
            self.attach_infer_var_diag(
                hole,
                span,
                "cannot infer the type this `_` stands for; write it in its place".to_string(),
            );
        }
    }

    /// Walk one body with `walk` in a context of its own, settled where it
    /// ends: a variable never outlives its body.
    pub(super) fn as_body<R>(&mut self, walk: impl FnOnce(&mut Self) -> R) -> R {
        assert!(
            !self.infer_holes.in_body && self.infer_holes.is_empty(),
            "a body starts outside every other, with no variable open"
        );
        self.infer_holes.in_body = true;
        let mark = FactsMark {
            body: self.sem.types.body.lens(),
            decls: DeclFactsLens::of(&self.sem.types),
            tuple_overlays: (self.sem.types.tuple_overlays.values())
                .map(Vec::len)
                .collect(),
            default_methods: self.sem.default_method_facts.len(),
        };
        let walked = walk(self);
        self.finalize_infer_holes(&mark);
        walked
    }

    /// Settle the body the facts past `since` were recorded by: report what
    /// nothing answered, close it to `error`, and substitute every answer into
    /// those facts.
    fn finalize_infer_holes(&mut self, since: &FactsMark) {
        if self.infer_holes.is_empty() {
            // The reports the body made reach no later body.
            self.infer_holes = InferHoleTable::default();
            return;
        }
        // A check queued after the last answer has not been asked yet.
        self.run_obligations();
        self.settle_pending_selections(Undecided::Report);
        self.absorb_reports();
        self.blame_bare_written_holes(|_| true);
        // A blamed class answered with a type still holding a variable is not
        // inferred either, unless that variable carries a blame of its own.
        let blamed = self.infer_holes.vars.blamed_classes();
        let unsolved: IndexSet<TypeId> = blamed
            .iter()
            .copied()
            .filter(|&class| {
                let answered = self.apply_infer_holes(class);
                let open = self.tysys.type_table.borrow().infer_vars_in(answered);
                open.iter()
                    .any(|&var| var == class || !blamed.contains(&var))
            })
            .collect();
        let blames = self.class_blames(&unsolved);
        self.report_uninferred(blames);
        for unwalked in std::mem::take(&mut self.infer_holes.unwalked_defaults) {
            let solved = unwalked
                .holes
                .iter()
                .all(|&hole| !self.infer_holes.vars.is_unanswered(hole));
            if solved {
                let _ = self.emit(TypeError::CannotInferType {
                    message: unwalked.message,
                    span: unwalked.span,
                });
            }
        }
        // Every variable still open has been reported or is covered, so it
        // stands as the error type, which every question takes, and the
        // queue runs once more at that.
        let holes = self.infer_holes.vars.holes.clone();
        for &hole in &holes {
            if self.infer_holes.vars.is_unanswered(hole) {
                self.close_infer_var(hole);
            }
        }
        assert_eq!(
            self.infer_holes.vars.len(),
            holes.len(),
            "closing a variable mints none"
        );
        let subst = self.pinned_hole_subst();
        self.verify_commitments(&subst);
        self.sweep_recorded_facts(&subst, since);
        self.infer_holes = InferHoleTable::default();
    }

    /// Link each associated-type equality of a bound on a slot the call
    /// deferred to a variable: the owner's answer settles the slot the right
    /// side names, or is held to the type it spells. `owners` declare the slots
    /// `names` and `args` list, in their order. `self_binding` is what `Self`
    /// means at the call.
    pub(super) fn link_deferred_assoc_equalities(
        &mut self,
        owners: &[GenericParam],
        names: &[ParamRef],
        args: &[TypeId],
        self_binding: Option<SelfBinding>,
        span: Span,
    ) {
        assert_eq!(names.len(), args.len(), "every slot is named");
        assert_eq!(owners.len(), args.len(), "every slot is declared");
        let space: ParamSpace = names.iter().cloned().zip(args.iter().copied()).collect();
        for (param, &owner) in owners.iter().zip(args) {
            for bound in &param.bounds {
                if bound.assoc_types.is_empty() {
                    continue;
                }
                // A bound naming no trait was reported where it is written.
                let Some(trait_) = self.tysys.resolutions.bound_decl(bound) else {
                    continue;
                };
                let written = (!bound.type_args.is_empty()).then(|| {
                    self.with_type_params_bound(&space, |e| {
                        e.under_self_binding(self_binding, |e| e.written_bound_args(bound))
                    })
                });
                for assoc in &bound.assoc_types {
                    let target = match assoc_target_slot(assoc, names, &self.tysys.resolutions) {
                        Some(slot) => args[slot],
                        None => match self.with_type_params_bound(&space, |e| {
                            e.equality_requirement(&assoc.ty, self_binding)
                        }) {
                            Some(required) => required,
                            None => continue,
                        },
                    };
                    self.infer_holes
                        .require(Obligation::Projection(ProjectionLink {
                            owner,
                            each_element: param.is_pack,
                            target,
                            trait_,
                            written: written.clone(),
                            assoc: assoc.clone(),
                            trait_name: bound.written_name(),
                            span,
                        }));
                }
            }
        }
    }

    /// Ask each obligation again until none progresses: asking one may settle
    /// a variable another reads, whether it is discharged or stays. What stays
    /// keeps its place ahead of what the asking queued.
    pub(super) fn run_obligations(&mut self) {
        self.absorb_reports();
        let mut pending = self.infer_holes.obligations.take();
        loop {
            let before = (pending.len(), self.infer_holes.vars.answers);
            pending = pending
                .into_iter()
                .filter_map(|obligation| self.ask(obligation))
                .collect();
            if (pending.len(), self.infer_holes.vars.answers) == before {
                break;
            }
        }
        let mut queue = self.infer_holes.obligations.borrow_mut();
        let queued = std::mem::replace(&mut *queue, pending);
        queue.extend(queued);
    }

    /// `obligation` asked at what its variables settled to so far: `None`
    /// where it held or was reported, itself where a variable it reads is
    /// still open.
    fn ask(&mut self, obligation: Obligation) -> Option<Obligation> {
        match obligation {
            Obligation::Bound {
                var,
                param,
                bound,
                span,
            } => {
                let Some(bound) = self.settled_bound(var, &bound) else {
                    self.settle_bound_args_by_sole_impl(var, &bound);
                    return Some(Obligation::Bound {
                        var,
                        param,
                        bound,
                        span,
                    });
                };
                let class = self
                    .infer_holes
                    .vars
                    .root_of(var)
                    .expect("a bound is required of a variable minted here");
                if !self
                    .infer_holes
                    .decided_bounds
                    .insert((class, bound.trait_.clone()))
                {
                    return None;
                }
                let solution = self.apply_infer_holes(var);
                // A still-parametric answer (a forwarded parameter) is checked
                // where its owner is monomorphized.
                if !self.tysys.type_table.borrow().contains_type_param(solution) {
                    for subject in self.tysys.bound_subjects(bound.each_element, solution) {
                        self.enforce_single_bound_args(
                            subject,
                            &bound.written,
                            Some(&bound.trait_),
                            &param,
                            span,
                        );
                    }
                }
                None
            }
            Obligation::Copyable {
                ty,
                is_pack,
                affine_params,
                param,
                span,
            } => {
                let answer = self.apply_infer_holes(ty);
                if !self.is_settled(answer) {
                    return Some(Obligation::Copyable {
                        ty,
                        is_pack,
                        affine_params,
                        param,
                        span,
                    });
                }
                if let Some(affine) = self.affine_subject(is_pack, answer, &affine_params) {
                    let type_name = self.tysys.type_id_to_string(affine);
                    let _ = self.emit(TypeError::AffineArgument {
                        type_name,
                        param_name: param,
                        span,
                    });
                }
                None
            }
            Obligation::Projection(link) => {
                (!self.settle_projection_link(&link)).then_some(Obligation::Projection(link))
            }
            Obligation::Selection(_) | Obligation::Pending(_) => Some(obligation),
            Obligation::Check(check) => {
                let actual = self.apply_infer_holes(check.actual);
                let expected = self.apply_infer_holes(check.expected);
                if !self.is_settled(actual) || !self.is_settled(expected) {
                    return Some(Obligation::Check(check));
                }
                if let Err(payload) = self.tysys.typecheck(actual, expected) {
                    let _ = self.emit((check.mismatch)(payload));
                }
                None
            }
        }
    }

    /// Whether every variable `ty` names is answered. An anonymous shape keeps
    /// naming its variables until the sweep rewrites it in place, so naming
    /// none is too strong.
    fn is_settled(&self, ty: TypeId) -> bool {
        self.tysys
            .type_table
            .borrow()
            .infer_vars_in(ty)
            .into_iter()
            .all(|var| !self.infer_holes.vars.is_unanswered(var))
    }

    /// `bound` with the variables its own arguments name answered
    /// (`T: Pick<U>` with `U` awaiting `1.5`), once they and `var` all are.
    fn settled_bound(&self, var: TypeId, bound: &DeclaredBound) -> Option<DeclaredBound> {
        if !self.is_settled(self.apply_infer_holes(var)) {
            return None;
        }
        let open = std::cell::Cell::new(false);
        let trait_ = bound.trait_.rewrite_args(&|name| match name.head() {
            TypeHead::Infer { var, .. } => {
                let hole = self.tysys.type_table.borrow_mut().make_infer_var(*var);
                let answer = self.apply_infer_holes(hole);
                open.set(open.get() || !self.is_settled(answer));
                Some(self.tysys.type_table.borrow().fq_type_name(answer))
            }
            _ => None,
        });
        (!open.get()).then(|| DeclaredBound {
            trait_,
            written: bound.written.clone(),
            each_element: bound.each_element,
        })
    }

    /// Settle the variables `bound`'s own arguments hold from the one impl of
    /// its trait that `var`'s answer has: with `impl Source<i32> for One`
    /// alone, `One: Source<?A>` says `?A` is `i32`, as `one.first()` says.
    /// Several impls leave them to the rest of the body.
    pub(super) fn settle_bound_args_by_sole_impl(&mut self, var: TypeId, bound: &DeclaredBound) {
        let subject = self.apply_infer_holes(var);
        if bound.each_element || !self.is_settled(subject) {
            return;
        }
        // A parameter has what its bounds in force say, and no block.
        let in_force: Vec<_> = self
            .carried_bounds(subject)
            .into_iter()
            .filter(|carried| self.trait_decl_of(carried) == bound.trait_.canonical())
            .collect();
        let impls = self.tysys.solver().bound_impls_at_any_args(
            &self.tysys,
            &self.annotate_ctx,
            subject,
            &bound.trait_,
        );
        // A bound in force and an impl are each an answer, so one of either
        // beside any of the other settles nothing.
        let offered: Vec<Option<TypeId>> =
            if let ([carried], BoundImpls::None) = (&in_force[..], &impls) {
                self.bound_args_where_written(carried, &ParamSpace::new())
                    .into_iter()
                    .map(Some)
                    .collect()
            } else if let ([], &BoundImpls::One(block)) = (&in_force[..], &impls) {
                let mut table = self.tysys.type_table.borrow_mut();
                let slots = table.impl_receiver_slots(block, subject);
                table
                    .impl_trait_args(block)
                    .to_vec()
                    .into_iter()
                    .map(|arg| {
                        let arg = table.substitute_type_params(arg, &slots);
                        // What the block leaves to its own parameters says nothing.
                        (!table.contains_type_param(arg)).then_some(arg)
                    })
                    .collect()
            } else {
                return;
            };
        for (asked, offered) in bound.trait_.args().iter().zip(offered) {
            if let Some(offered) = offered {
                self.equate_open_parts(asked, offered);
            }
        }
    }

    /// Equate each variable `asked` writes with the part of `offered` at its
    /// position, where the two agree on every head above it.
    fn equate_open_parts(&mut self, asked: &FqTypeName, offered: TypeId) {
        let mut offered = offered;
        for kind in asked.references() {
            let ((ResolvedType::Ref(behind), RefKind::ImmutableReference)
            | (ResolvedType::MutRef(behind), RefKind::Mut)) =
                (self.tysys.type_table.borrow().get(offered).clone(), kind)
            else {
                return;
            };
            offered = behind;
        }
        if let TypeHead::Infer { var, .. } = asked.head() {
            let hole = self.tysys.type_table.borrow_mut().make_infer_var(*var);
            // A literal's type is a default that settles last, so the bound
            // chooses it first where the literal can take the type. One still
            // waiting for its call's own arguments is left to them.
            let root = self.apply_infer_holes(hole);
            let literal = self
                .infer_holes
                .defaulting_literals
                .iter()
                .find(|(var, _)| *var == root)
                .map(|(_, class)| class.clone());
            let takes = match literal {
                Some(class) => self.class_admits(offered, &class),
                None => !self.annotate_ctx.literals_wait_at(hole),
            };
            if takes {
                self.equate(offered, hole);
            }
            return;
        }
        let (name, parts) = {
            let table = self.tysys.type_table.borrow();
            let parts = if let ResolvedType::Function {
                params,
                return_type,
                ..
            } = table.get(offered)
            {
                params.iter().copied().chain([*return_type]).collect()
            } else if let Some(elems) = table.as_tuple(offered) {
                elems
            } else {
                table.nominal_type_args(offered).unwrap_or_default()
            };
            (table.fq_type_name(offered), parts)
        };
        if !name.references().is_empty() {
            return;
        }
        // A function type's head is its whole signature, which is where its
        // variables are, so two of them agree on the rest of it.
        let asked_parts = if let (
            TypeHead::Function {
                is_mut,
                signature,
                effects,
            },
            TypeHead::Function {
                is_mut: offered_mut,
                effects: offered_effects,
                ..
            },
        ) = (asked.head(), name.head())
        {
            if is_mut != offered_mut || effects != offered_effects {
                return;
            }
            &signature[..]
        } else if name.head() == asked.head() {
            asked.args()
        } else {
            return;
        };
        if asked_parts.len() == parts.len() {
            for (asked, part) in asked_parts.iter().zip(parts) {
                self.equate_open_parts(asked, part);
            }
        }
    }

    /// Whether `link` is decided: its owner answered, and the link settled
    /// for it, or for each element where the owner is a pack, with nothing of
    /// a target left open.
    fn settle_projection_link(&mut self, link: &ProjectionLink) -> bool {
        let owner = self.apply_infer_holes(link.owner);
        if !self.tysys.is_usable_answer(owner) {
            return false;
        }
        let written: Option<Vec<TypeId>> = link
            .written
            .as_ref()
            .map(|written| written.iter().map(|&t| self.apply_infer_holes(t)).collect());
        if written.as_ref().is_some_and(|written| {
            let tt = self.tysys.type_table.borrow();
            written.iter().any(|&t| tt.contains_undecided(t))
        }) {
            return false;
        }
        let mut decided = true;
        for subject in self.tysys.bound_subjects(link.each_element, owner) {
            decided &= self.settle_projection_of(link, subject, written.as_deref());
        }
        decided
    }

    /// `link` at the answered `owner`: the target is answered by the owner's
    /// projection or held to it, and whether that is decided. A projection
    /// that is not there, or not a tuple a pack takes, leaves the target to
    /// its own report.
    fn settle_projection_of(
        &mut self,
        link: &ProjectionLink,
        owner: TypeId,
        written: Option<&[TypeId]>,
    ) -> bool {
        let trait_args =
            written.and_then(|written| self.trait_args_at(link.trait_, owner, written));
        let forwarded = self
            .scope_type_param_ids()
            .owns(&self.tysys.type_table.borrow(), owner);
        let Some(answer) =
            self.bound_projection(owner, forwarded, link.trait_, trait_args, &link.assoc)
        else {
            return true;
        };
        self.equate(answer, link.target);
        // Solving fills the holes and leaves the rest of the target as
        // written, which the answer must still match: `[i32, U]` against
        // `[bool, String]` settles `U` and misses at `i32`. A target still
        // open there (`[String, Box<U>]` against `[i32, bool]`) is held to it
        // once it settles.
        let target = self.apply_infer_holes(link.target);
        if self.tysys.type_table.borrow().contains_undecided(target) {
            return false;
        }
        if target == answer || says_nothing(target) || !self.decides_equality(target, answer) {
            return true;
        }
        let tt = self.tysys.type_table.borrow();
        let (type_name, expected, actual) = (tt.name(owner), tt.name(target), tt.name(answer));
        drop(tt);
        let _ = self.emit(TypeError::AssocTypeBoundNotSatisfied {
            type_name,
            trait_name: link.trait_name.clone(),
            assoc_name: link.assoc.name.clone(),
            expected,
            actual,
            span: link.span,
        });
        true
    }

    /// Ask each commitment again at what its variables settled to; nothing
    /// else is left queued once every variable is. A variable no hole holds
    /// belongs to no call left to answer it.
    fn verify_commitments(&self, subst: &IndexMap<InferVarId, TypeId>) {
        let commitments: Vec<Commitment> = self
            .infer_holes
            .obligations
            .take()
            .into_iter()
            .filter_map(|obligation| match obligation {
                Obligation::Selection(commitment) => Some(commitment),
                // An owner closed behind a report has no projection to give.
                Obligation::Projection(_) => None,
                Obligation::Bound { .. } | Obligation::Copyable { .. } | Obligation::Check(_) => {
                    unreachable!("a settled variable decides a bound or a check")
                }
                Obligation::Pending(_) => {
                    unreachable!("a pending selection is made or reported before the sweep")
                }
            })
            .collect();
        let Some(solver) = self.tysys.solver.as_ref() else {
            assert!(commitments.is_empty(), "a commitment comes from the solver");
            return;
        };
        if self.logger.has_errors() {
            return;
        }
        let bridge = solver.borrow();
        let solution = |var: InferVarId| subst.get(&var).copied().unwrap_or(TypeTable::ERROR);
        for commitment in &commitments {
            bridge.check_commitment(&self.tysys, commitment, &solution);
        }
    }

    /// Substitute holes through every fact the body recorded past `since` that
    /// can carry a `TypeId`. Trait default-method bodies record into their own
    /// facts in `default_method_facts`, not the main `types`; sweep those too,
    /// or a hole minted in a default-method body leaks past reify.
    fn sweep_recorded_facts(&mut self, subst: &IndexMap<InferVarId, TypeId>, since: &FactsMark) {
        if subst.is_empty() {
            return;
        }
        let sweep = Sweep {
            subst,
            selections: &std::mem::take(&mut self.infer_holes.selections),
        };
        let mut tt = self.tysys.type_table.borrow_mut();
        let types = &mut self.sem.types;
        Self::sweep_decl_facts(&mut tt, types, since.decls, subst);
        let mut body = types.body.split_off(since.body);
        Self::sweep_body_facts(&mut tt, &mut body, &sweep);
        types.body.append(body);
        Self::sweep_tuple_overlays(&mut tt, types, &since.tuple_overlays, &sweep);
        for facts in self
            .sem
            .default_method_facts
            .values_mut()
            .skip(since.default_methods)
        {
            Self::sweep_decl_facts(&mut tt, &mut facts.types, DeclFactsLens::default(), subst);
            Self::sweep_body_facts(&mut tt, &mut facts.types.body, &sweep);
            Self::sweep_tuple_overlays(&mut tt, &mut facts.types, &[], &sweep);
        }
    }

    /// Substitute `subst` through the facts a declaration records once, past
    /// `since`.
    fn sweep_decl_facts(
        tt: &mut TypeTable,
        types: &mut TypeAnnotations,
        since: DeclFactsLens,
        subst: &IndexMap<InferVarId, TypeId>,
    ) {
        for (map, since) in [
            (&mut types.fn_return_types, since.fn_return_types),
            (
                &mut types.function_task_returns,
                since.function_task_returns,
            ),
        ] {
            for t in map.values_mut().skip(since) {
                *t = sub(tt, *t, subst);
            }
        }
        for (map, since) in [
            (&mut types.fn_param_types, since.fn_param_types),
            (&mut types.fn_affine_params, since.fn_affine_params),
            (&mut types.struct_field_types, since.struct_field_types),
        ] {
            for v in map.values_mut().skip(since) {
                sub_vec(tt, v, subst);
            }
        }
        for i in types.impl_facts.values_mut().skip(since.impl_facts) {
            sub_vec(tt, &mut i.trait_type_args, subst);
        }
    }

    /// The tuple `for-of` unrollings past the first `since[i]` of loop `i`. A
    /// loop in a default argument is unrolled again by each site omitting it.
    fn sweep_tuple_overlays(
        tt: &mut TypeTable,
        types: &mut TypeAnnotations,
        since: &[usize],
        sweep: &Sweep<'_>,
    ) {
        for (i, unrollings) in types.tuple_overlays.values_mut().enumerate() {
            let settled = since.get(i).copied().unwrap_or(0);
            for overlay in unrollings.iter_mut().skip(settled).flatten() {
                Self::sweep_body_facts(tt, overlay, sweep);
            }
        }
    }

    /// Every body fact kind that can hold a `TypeId` has one substitution
    /// below; the module's own walk, each unrolled tuple `for-of` element and
    /// each default-argument walk hold the same kinds and sweep through the
    /// same list.
    fn sweep_body_facts(tt: &mut TypeTable, facts: &mut BodyFacts, sweep: &Sweep<'_>) {
        let subst = sweep.subst;
        sub_map(tt, &mut facts.expression_types, subst);
        sub_map(tt, &mut facts.local_types, subst);
        sub_map(tt, &mut facts.let_annotated_types, subst);
        sub_map(tt, &mut facts.pattern_ascriptions, subst);
        sub_map(tt, &mut facts.destructured_types, subst);
        sub_map(tt, &mut facts.range_element_hints, subst);
        for c in facts.list_view_conversions.values_mut() {
            c.view = sub(tt, c.view, subst);
            c.answering = sub(tt, c.answering, subst);
        }
        sub_map(tt, &mut facts.widenings, subst);
        sub_vec_map(tt, &mut facts.call_param_types, subst);
        for gi in facts.generic_instantiations.values_mut() {
            sub_generic_instantiation(tt, gi, subst);
        }
        for md in facts.method_dispatch.values_mut() {
            if let Some(id) = md.selection.take()
                && let Some(selected) = &sweep.selections[id]
            {
                md.method_def = selected.method_def;
                md.function_ref = selected.function_ref.clone();
            }
            sub_method_dispatch(tt, md, subst);
        }
        for sd in facts.static_method_dispatch.values_mut() {
            sub_static_dispatch(tt, sd, subst);
        }
        for c in facts.coercions.values_mut() {
            c.target_type = sub(tt, c.target_type, subst);
        }
        for od in facts
            .operator_dispatch
            .values_mut()
            .chain(facts.index_assign_dispatch.values_mut())
            .chain(facts.index_swap_dispatch.values_mut())
        {
            sub_operator_dispatch(tt, od, subst);
        }
        for f in facts.for_of_iterator.values_mut() {
            sub_for_of(tt, f, subst);
        }
        for walk in facts.for_of_index_walks.values_mut() {
            walk.item_type = sub(tt, walk.item_type, subst);
            sub_monomorph(tt, &mut walk.len.func, subst);
            sub_operator_dispatch(tt, &mut walk.read, subst);
            sub_operator_dispatch(tt, &mut walk.write, subst);
        }
        for cc in facts.closure_captures.values_mut() {
            sub_closure_captures(tt, cc, subst);
        }
        for h in facts.handler_bindings.values_mut() {
            h.handler_type = sub(tt, h.handler_type, subst);
            for e in &mut h.effects {
                sub_vec(tt, &mut e.trait_type_args, subst);
            }
        }
        for sc in facts.sequence_coercions.values_mut() {
            sub_sequence_coercion(tt, sc, subst);
        }
        for kv in facts.key_value_coercions.values_mut() {
            sub_key_value_coercion(tt, kv, subst);
        }
        for call in facts.literal_conversions.values_mut() {
            sub_literal_from_call(tt, call, subst);
        }
        // A call's omitted arguments produced facts of their own, and one of
        // those may itself be a defaulted call. Swept here rather than beside
        // the caller's walk, so reaching a walk reaches everything under it.
        for overlay in facts.default_overlays.values_mut() {
            Self::sweep_body_facts(tt, overlay, sweep);
        }
    }
}

/// Where the facts stood when a body's walk began, so the sweep at its end
/// reaches only what the walk recorded.
pub(super) struct FactsMark {
    body: BodyFactsLens,
    decls: DeclFactsLens,
    /// How many times each tuple `for-of` had been unrolled.
    tuple_overlays: Vec<usize>,
    default_methods: usize,
}

/// The length of each map of the facts a declaration records once.
#[derive(Clone, Copy, Default)]
struct DeclFactsLens {
    fn_return_types: usize,
    function_task_returns: usize,
    fn_param_types: usize,
    fn_affine_params: usize,
    struct_field_types: usize,
    impl_facts: usize,
}

impl DeclFactsLens {
    fn of(types: &TypeAnnotations) -> Self {
        Self {
            fn_return_types: types.fn_return_types.len(),
            function_task_returns: types.function_task_returns.len(),
            fn_param_types: types.fn_param_types.len(),
            fn_affine_params: types.fn_affine_params.len(),
            struct_field_types: types.struct_field_types.len(),
            impl_facts: types.impl_facts.len(),
        }
    }
}

/// What a body's sweep writes into the recorded facts: each variable's
/// answer, and the dispatch each pending selection made.
struct Sweep<'a> {
    subst: &'a IndexMap<InferVarId, TypeId>,
    selections: &'a [Option<SelectedDispatch>],
}

impl TypeSystem {
    /// Whether `answer` can stand as a variable's solution: not another variable,
    /// nor one of the [`says_nothing`] types.
    pub(super) fn is_usable_answer(&self, answer: TypeId) -> bool {
        !says_nothing(answer) && !self.type_table.borrow().contains_infer_var(answer)
    }
}

/// Whether `t` names a type parameter `scope` does not own.
fn names_foreign_param(tt: &TypeTable, t: TypeId, scope: &ScopeParams) -> bool {
    match tt.get(t) {
        ResolvedType::TypeParam { id, .. } | ResolvedType::TypePack { id, .. } => {
            !scope.ids().contains(id)
        }
        _ => tt.any_constituent(t, &mut |c| names_foreign_param(tt, c, scope)),
    }
}

/// Whether `answer` is `never` / `unknown` / `error`, which every check
/// accepts anywhere, so it answers no variable.
pub(super) fn says_nothing(answer: TypeId) -> bool {
    matches!(
        answer,
        TypeTable::NEVER | TypeTable::UNKNOWN | TypeTable::ERROR
    )
}

/// What an unsolved parameter of the `kind` spelled `name` reports.
pub(super) fn uninferable_type_param(kind: &str, name: &str) -> String {
    format!(
        "cannot infer type parameter of {kind} `{name}`; add a turbofish (`{name}::<...>::…`) or a type annotation"
    )
}

/// One substitution per fact kind that can hold a `TypeId`, shared by the
/// top-level sweep and the per-iteration overlay sweep so neither can hold a
/// list the other has outgrown.
fn sub(tt: &mut TypeTable, t: TypeId, subst: &IndexMap<InferVarId, TypeId>) -> TypeId {
    tt.substitute_infer_vars(t, subst)
}

fn sub_vec(tt: &mut TypeTable, v: &mut [TypeId], subst: &IndexMap<InferVarId, TypeId>) {
    for t in v.iter_mut() {
        *t = sub(tt, *t, subst);
    }
}

fn sub_map(
    tt: &mut TypeTable,
    m: &mut IndexMap<AstId, TypeId>,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    for t in m.values_mut() {
        *t = sub(tt, *t, subst);
    }
}

fn sub_vec_map(
    tt: &mut TypeTable,
    m: &mut IndexMap<AstId, Vec<TypeId>>,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    for v in m.values_mut() {
        sub_vec(tt, v, subst);
    }
}

fn sub_monomorph(
    tt: &mut TypeTable,
    f: &mut tir::FunctionRef,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    if let Some(mi) = f.monomorph_info.as_mut() {
        sub_vec(tt, &mut mi.impl_type_args, subst);
        sub_vec(tt, &mut mi.method_type_args, subst);
    }
}

/// A struct-literal mangled name (`Wrapper<?0>`) is frozen before the variable
/// is solved, and reify emits it verbatim, so it is rebuilt from the swept
/// instance type.
fn sub_generic_instantiation(
    tt: &mut TypeTable,
    gi: &mut GenericInstantiation,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    sub_vec(tt, &mut gi.type_args, subst);
    gi.instance_type = sub(tt, gi.instance_type, subst);
    if let Some(name) = gi.mangled_name.as_mut()
        && let ResolvedType::Struct {
            def: StructDef::Decl(def),
            type_args,
        } = tt.get(gi.instance_type).clone()
        && let base = tt.decl_name(def).to_string()
    {
        let arg_names: Vec<String> = type_args
            .iter()
            .map(|&t| tt.mangle_type_arg_for_generic(t))
            .collect();
        *name = mangle_generic_name(&base, &arg_names);
    }
}

fn sub_method_dispatch(
    tt: &mut TypeTable,
    md: &mut MethodDispatch,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    md.return_type = sub(tt, md.return_type, subst);
    sub_vec(tt, &mut md.method_type_args, subst);
    sub_monomorph(tt, &mut md.function_ref, subst);
}

fn sub_static_dispatch(
    tt: &mut TypeTable,
    sd: &mut StaticMethodDispatch,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    sub_vec(tt, &mut sd.type_args, subst);
    sub_vec(tt, &mut sd.param_types, subst);
    sub_monomorph(tt, &mut sd.function_ref, subst);
}

fn sub_operator_dispatch(
    tt: &mut TypeTable,
    od: &mut OperatorDispatch,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    od.return_type = sub(tt, od.return_type, subst);
    sub_monomorph(tt, &mut od.function_ref, subst);
}

fn sub_for_of(tt: &mut TypeTable, f: &mut ForOfIteratorInfo, subst: &IndexMap<InferVarId, TypeId>) {
    f.item_type = sub(tt, f.item_type, subst);
    f.iter_type = sub(tt, f.iter_type, subst);
    if let Some(start) = &mut f.start {
        sub_monomorph(tt, &mut start.func, subst);
    }
    sub_monomorph(tt, &mut f.next, subst);
}

fn sub_closure_captures(
    tt: &mut TypeTable,
    cc: &mut ClosureCaptureInfo,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    for c in &mut cc.captures {
        c.type_id = sub(tt, c.type_id, subst);
    }
    for m in &mut cc.mut_captures {
        m.inner_type = sub(tt, m.inner_type, subst);
        m.ref_type = sub(tt, m.ref_type, subst);
    }
    cc.return_type = sub(tt, cc.return_type, subst);
}

fn sub_sequence_coercion(
    tt: &mut TypeTable,
    sc: &mut SequenceCoercionFacts,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    sc.element_type = sub(tt, sc.element_type, subst);
    sc.newtype_cast_to = sc.newtype_cast_to.map(|t| sub(tt, t, subst));
    sub_literal_from_call(tt, &mut sc.call, subst);
}

fn sub_literal_from_call(
    tt: &mut TypeTable,
    call: &mut LiteralFromCall,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    call.from_type = sub(tt, call.from_type, subst);
    call.output_type = sub(tt, call.output_type, subst);
    sub_literal_callee(tt, &mut call.callee, subst);
}

fn sub_literal_callee(
    tt: &mut TypeTable,
    callee: &mut LiteralCallee,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    sub_vec(tt, &mut callee.type_arg_ids, subst);
    callee.remangle(tt);
}

fn sub_key_value_coercion(
    tt: &mut TypeTable,
    kv: &mut KeyValueCoercionFacts,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    kv.value_type = sub(tt, kv.value_type, subst);
    kv.pair_type = sub(tt, kv.pair_type, subst);
    kv.newtype_cast_to = kv.newtype_cast_to.map(|t| sub(tt, t, subst));
    sub_literal_from_call(tt, &mut kv.call, subst);
    if let Some(spread) = kv.spread.as_mut() {
        sub_literal_callee(tt, spread, subst);
    }
}
