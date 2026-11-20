//! The module's inference context. A type the source leaves to inference —
//! `p.get()` in `p.get().unwrap()` — is an [`ResolvedType::InferVar`] that flows
//! on until a check [`Elaborator::equate`]s it with a type. What a variable
//! leaves undecided (a bound, a projection, a selection, a check) waits as an
//! [`Obligation`], asked again as variables settle. The module-end sweep reports
//! what nothing settled and substitutes the rest.

use crate::compiler_host::{CompilerHost, DiagnosticSpan};
use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{InferVarId, ResolvedType, StructDef, TypeId};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::call::assoc_target_slot;
use super::coercion::more_general_default;
use super::scope::{ParamRef, ScopeParams};
use super::solver_bridge::Commitment;
use super::trait_query::{SelfBinding, spelled_at};
use super::type_resolution::ParamSpace;
use super::types::TypeError;
use super::tysys::TypeSystem;
use crate::ast::{AssocTypeBound, AstId, Expr, GenericParam};
use crate::elaborator::sem::TypeAnnotations;
use crate::elaborator::sem::types::{
    BodyFacts, ClosureCaptureInfo, ForOfIteratorInfo, GenericInstantiation, KeyValueCoercionFacts,
    LiteralCallee, LiteralFromCall, MethodDispatch, OperatorDispatch, SequenceCoercionFacts,
    StaticMethodDispatch,
};
use crate::elaborator::typecheck::{TypeMismatchPayload, meetings};
use crate::elaborator::types::VariantInfo;
use crate::name::{FqTraitName, FqTypeName, TypeHead, mangle_generic_name, written_param_name};
use crate::tir;

/// Per-module registry of inference holes and their (eventual) solutions.
#[derive(Default)]
pub(crate) struct InferHoleTable {
    vars: VarStore,
    /// Hole `TypeId` → what it says if it is never solved.
    diags: IndexMap<TypeId, Blame>,
    /// Where a type mismatch was reported. The mismatch says what was wrong
    /// there, so an unsolved hole inside one reports nothing more. Recorded
    /// where a check reads the table, so behind a cell.
    mismatches: std::cell::RefCell<Vec<DiagnosticSpan>>,
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
    /// An associated-type equality of a bound on a slot a variable fills.
    Projection(ProjectionLink),
    /// An impl a call committed to while its receiver held variables. Asked
    /// only once the body ends: a reported error can be the very bound it
    /// left, so only a clean program owes the answer.
    Selection(Commitment),
    /// A type check that admitted a value while a variable on either side
    /// left it undecided.
    Check(DeferredCheck),
    /// A numeric literal whose type is `var`: it must fit the type `var`
    /// settles to, as it would have fit that type written as its slot.
    Literal { expr: Expr, var: TypeId },
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
    owner: TypeId,
    /// A pack's bound holds of each element of the tuple the pack answers.
    each_element: bool,
    /// The right side at the call: the slot it names, or the type it spells.
    target: TypeId,
    trait_: DefId,
    /// The bound's own arguments as the call read them, `None` where it
    /// writes none.
    written: Option<Vec<TypeId>>,
    assoc: AssocTypeBound,
    trait_name: String,
    span: Span,
}

impl InferHoleTable {
    pub(super) fn note_mismatch(&self, span: DiagnosticSpan) {
        self.mismatches.borrow_mut().push(span);
    }

    pub(super) fn require(&self, obligation: Obligation) {
        self.obligations.borrow_mut().push(obligation);
    }
}

/// Whether the reported `outer` covers `inner`, both in one parse.
fn covers(outer: &DiagnosticSpan, inner: &Span) -> bool {
    let end = (
        outer.end_line.unwrap_or(outer.line),
        outer.end_column.unwrap_or(outer.column),
    );
    outer.space == inner.space
        && (outer.line, outer.column) <= (inner.line, inner.column)
        && (inner.end_line, inner.end_column) <= end
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
        let named = params
            .iter()
            .map(|p| format!("`{}`", written_param_name(p)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("cannot infer type parameter {named} {}", self.owner)
    }
}

impl InferHoleTable {
    pub(super) fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.obligations.borrow().is_empty()
    }
}

/// The module's inference variables, as a union-find: a variable answered by
/// another follows it, and only a root holds a binding.
#[derive(Default)]
struct VarStore {
    /// Each variable's type, in mint order.
    holes: Vec<TypeId>,
    index: IndexMap<TypeId, usize>,
    parent: Vec<usize>,
    /// A root's answer, `None` while it has none.
    binding: Vec<Option<TypeId>>,
    /// The variables standing for a pack: a list of types, spliced where a
    /// tuple holds it, so never one element's type.
    packs: IndexSet<TypeId>,
    /// A root a numeric literal met, with the type it takes where nothing
    /// else settles it: `i32`, or `f64` once a float literal joined it.
    literal_defaults: IndexMap<usize, TypeId>,
}

impl VarStore {
    fn is_empty(&self) -> bool {
        self.holes.is_empty()
    }

    fn len(&self) -> usize {
        self.holes.len()
    }

    fn mint(&mut self, hole: TypeId) {
        let i = self.holes.len();
        assert!(
            self.index.insert(hole, i).is_none(),
            "inference variable {hole:?} minted twice"
        );
        self.holes.push(hole);
        self.parent.push(i);
        self.binding.push(None);
    }

    fn contains(&self, hole: TypeId) -> bool {
        self.index.contains_key(&hole)
    }

    fn root(&self, mut i: usize) -> usize {
        while self.parent[i] != i {
            i = self.parent[i];
        }
        i
    }

    /// Whether `t` is a variable whose class a pack variable joined.
    fn is_pack(&self, t: TypeId) -> bool {
        let Some(class) = self.root_of(t) else {
            return false;
        };
        self.packs.iter().any(|&p| self.root_of(p) == Some(class))
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
                if let Some(default) = self.literal_defaults.shift_remove(&i) {
                    self.add_literal_default(root, default);
                }
            }
            None => self.binding[i] = Some(answer),
        }
    }

    /// Record that a literal defaulting to `default` met root `root`; the
    /// class takes the more general of its literals' defaults.
    fn add_literal_default(&mut self, root: usize, default: TypeId) {
        let held = self.literal_defaults.entry(root).or_insert(default);
        *held = more_general_default(*held, default);
    }

    /// The default of `hole`'s class while it is unbound and a literal met it.
    fn literal_default(&self, hole: TypeId) -> Option<TypeId> {
        let root = self.root(*self.index.get(&hole)?);
        if self.binding[root].is_some() {
            return None;
        }
        self.literal_defaults.get(&root).copied()
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Mint a fresh inference hole, remembering the diagnostic to raise if it
    /// is never solved (mirrors the immediate "cannot infer" error so the
    /// user-facing message is unchanged, only deferred).
    pub(super) fn mint_infer_hole(
        &mut self,
        span: Span,
        message: String,
        param_name: String,
        bounds: Vec<DeclaredBound>,
    ) -> TypeId {
        let hole = self.mint_infer_var_named(&param_name);
        self.attach_infer_var_diag(hole, span, message);
        self.attach_infer_var_bounds(hole, param_name, bounds, span);
        hole
    }

    /// Mark `var` as standing for a pack: it is answered by a tuple, never by
    /// the type of one element of the tuple holding it.
    pub(super) fn mark_pack_var(&mut self, var: TypeId) {
        assert!(
            self.infer_holes.vars.contains(var),
            "a pack variable is minted here"
        );
        self.infer_holes.vars.packs.insert(var);
    }

    /// [`Self::mint_infer_hole`] for a call's type argument `slot`. A variable
    /// inference already put there takes the diagnostic and the bounds: a
    /// second one would orphan it, which the sweep would then pin to `error`
    /// behind a diagnostic nobody asked for.
    pub(super) fn defer_slot(
        &mut self,
        slot: Option<TypeId>,
        span: Span,
        message: String,
        param_name: String,
        bounds: Vec<DeclaredBound>,
    ) -> TypeId {
        let existing = slot.filter(|&t| self.tysys.type_table.borrow().contains_infer_var(t));
        let Some(var) = existing else {
            return self.mint_infer_hole(span, message, param_name, bounds);
        };
        self.attach_infer_var_diag(var, span, message);
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
    pub(super) fn declared_bounds(
        &mut self,
        param: &GenericParam,
        self_binding: Option<SelfBinding>,
        impl_args: &[(ParamRef, TypeId)],
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
    /// A use site that may discard its instantiation — inference runs twice
    /// for a partial turbofish — mints bare and attaches the diagnostic only
    /// to the variables it commits to. A variable nobody kept then reports
    /// nothing.
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
        self.infer_holes.vars.mint(hole);
        hole
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
        span: Span,
    ) {
        let bounds = self.declared_bounds(param, self_binding, impl_args);
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
    /// wins, matching the solutions map's keep-the-first policy.
    pub(super) fn attach_infer_var_diag(&mut self, var: TypeId, span: Span, message: String) {
        self.infer_holes.diags.entry(var).or_insert(Blame {
            span,
            param: None,
            owner: message,
        });
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
        let vars = tt.infer_vars_in(ty);
        !vars.is_empty()
            && vars.iter().all(|var| {
                self.infer_holes.diags.contains_key(var)
                    && self.infer_holes.vars.is_unanswered(*var)
            })
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
        self.infer_holes.diags.entry(var).or_insert(Blame {
            span,
            param: Some(param),
            owner,
        });
    }

    /// Defer a variant constructor whose type arguments did not resolve here.
    ///
    /// `infer_variant_type_args` answers with the bare declaration type when
    /// nothing pinned the parameters. That type has no case types registered,
    /// so it dies in WIR build — yet a sibling field
    /// (`Paired { v: Option::None, k: 1 }`), an annotation, or a later use can
    /// still pin it. Mint a hole per parameter so a real sink may solve it and
    /// `finalize_infer_holes` reports only what never was.
    pub(super) fn defer_uninferable_variant(
        &mut self,
        type_id: TypeId,
        variant_name: &str,
        variant_info: &VariantInfo,
        span: Span,
    ) -> TypeId {
        if variant_info.type_param_type_ids.is_empty() {
            return type_id;
        }
        let is_bare_decl = self
            .tysys
            .type_table
            .borrow()
            .type_id_of_decl(variant_info.defined_at)
            == type_id;
        if !is_bare_decl {
            return type_id;
        }
        let message = uninferable_type_param("variant", variant_name);
        let holes: Vec<TypeId> = variant_info
            .type_params
            .iter()
            .map(|param| {
                self.mint_infer_hole(span, message.clone(), param.name.clone(), Vec::new())
            })
            .collect();
        assert_eq!(
            holes.len(),
            variant_info.type_param_type_ids.len(),
            "a variant's parameters and their ids are one list"
        );
        {
            let def = self
                .tysys
                .resolutions
                .defs()
                .def_at(variant_info.defined_at);
            self.tysys
                .type_table
                .borrow_mut()
                .make_generic_instance(def, holes)
        }
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
        self.tysys.is_unbound_type_param(t)
            && !scope_params.owns(&self.tysys.type_table.borrow(), t)
    }

    pub(super) fn type_has_infer_hole(&self, ty: TypeId) -> bool {
        if self.infer_holes.is_empty() {
            return false;
        }
        self.tysys.type_table.borrow().contains_infer_var(ty)
    }

    /// Pin `var` to the error type: a reported diagnostic already covers it,
    /// and nothing later may settle it.
    pub(super) fn close_infer_var(&mut self, var: TypeId) {
        self.answer_infer_var(var, TypeTable::ERROR);
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
        let class = self.infer_holes.vars.root_of(var);
        let occurs = self
            .tysys
            .type_table
            .borrow()
            .infer_vars_in(answer)
            .into_iter()
            .any(|v| self.infer_holes.vars.root_of(v) == class);
        if occurs {
            return false;
        }
        self.infer_holes.vars.answer_with(var, answer);
        self.run_obligations();
        true
    }

    /// Make a value's type `actual` one with its slot's `expected` wherever a
    /// variable on either side is what leaves the check undecided: the
    /// variable is bound to the other side, or joins it where that is a
    /// variable too. A side naming a callee's own parameter binds nothing,
    /// since the callee's solve answers it.
    pub(super) fn equate(&mut self, actual: TypeId, expected: TypeId) {
        if !self.type_has_infer_hole(actual) && !self.type_has_infer_hole(expected) {
            return;
        }
        let scope = self.scope_type_param_ids();
        loop {
            let mut pairs = Vec::new();
            meetings(
                self.apply_infer_holes(actual),
                self.apply_infer_holes(expected),
                &self.tysys.type_table.borrow(),
                &|t| self.infer_holes.vars.is_pack(t),
                &mut pairs,
            );
            if !pairs
                .into_iter()
                .any(|(a, e)| self.settle_meeting(a, e, &scope))
            {
                return;
            }
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
            // The module being resolved owns every hole in the table.
            let _ = self.emit(TypeError::CannotInferType {
                message: blame.message(&unique),
                span: blame.span,
            });
        }
    }

    /// Take the blames of `classes`, each by the earliest variable joined
    /// into it that carries one, so a class reports once.
    fn class_blames(&mut self, classes: &IndexSet<TypeId>) -> Vec<Blame> {
        let picked: Vec<(TypeId, TypeId)> = self
            .infer_holes
            .diags
            .keys()
            .map(|&hole| {
                let root = self.infer_holes.vars.root_of(hole);
                (hole, root.expect("a blamed variable is minted here"))
            })
            .filter(|(_, root)| classes.contains(root))
            .collect();
        let mut reported: IndexSet<TypeId> = IndexSet::default();
        let mut blames = Vec::new();
        for (hole, root) in picked {
            let blame = self.infer_holes.diags.shift_remove(&hole).unwrap();
            if reported.insert(root) {
                blames.push(blame);
            }
        }
        blames
    }

    /// Report `vars` as never answered now, then pin each to the error type so
    /// nothing later settles them.
    pub(super) fn close_uninferred(&mut self, vars: &[TypeId]) {
        let classes: IndexSet<TypeId> = vars
            .iter()
            .filter_map(|&var| self.infer_holes.vars.root_of(var))
            .collect();
        let blames = self.class_blames(&classes);
        self.report_uninferred(blames);
        for &var in vars {
            self.close_infer_var(var);
        }
    }

    /// End-of-module finalize: raise diagnostics for unsolved holes and
    /// substitute every hole (solved → concrete, unsolved → `error`) into all
    /// recorded facts that can carry one.
    pub(super) fn finalize_infer_holes(&mut self) {
        if self.infer_holes.is_empty() {
            return;
        }
        // A variable only literals met takes their default, and what that
        // settles is asked before anything left open is reported.
        for hole in self.infer_holes.vars.holes.clone() {
            self.literal_head(hole);
        }
        let mismatches = self.infer_holes.mismatches.take();
        let covered: IndexSet<TypeId> = self
            .infer_holes
            .diags
            .iter()
            .filter(|(_, blame)| mismatches.iter().any(|outer| covers(outer, &blame.span)))
            .filter_map(|(&hole, _)| self.infer_holes.vars.root_of(hole))
            .collect();
        let unsolved: IndexSet<TypeId> = self
            .infer_holes
            .diags
            .keys()
            .filter(|&&hole| self.infer_holes.vars.is_unanswered(hole))
            .filter_map(|&hole| self.infer_holes.vars.root_of(hole))
            .filter(|root| !covered.contains(root))
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
        for hole in self.infer_holes.vars.holes.clone() {
            if self.infer_holes.vars.is_unanswered(hole) {
                self.close_infer_var(hole);
            }
        }
        let subst = self.pinned_hole_subst();
        self.verify_commitments(&subst);
        self.sweep_recorded_facts(&subst);
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
            let owner = self.apply_infer_holes(owner);
            if !self.tysys.type_table.borrow().is_infer_var(owner) {
                continue;
            }
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

    /// Ask each obligation again until none progresses: discharging one may
    /// settle a variable another reads. What stays keeps its place ahead of
    /// what the asking queued.
    pub(super) fn run_obligations(&mut self) {
        let mut pending = self.infer_holes.obligations.take();
        loop {
            let before = pending.len();
            pending = pending
                .into_iter()
                .filter_map(|obligation| self.ask(obligation))
                .collect();
            if pending.len() == before {
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
            Obligation::Projection(link) => {
                (!self.settle_projection_link(&link)).then_some(Obligation::Projection(link))
            }
            Obligation::Selection(_) => Some(obligation),
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
            Obligation::Literal { expr, var } => {
                let answer = self.apply_infer_holes(var);
                if self.tysys.type_table.borrow().is_infer_var(answer) {
                    return Some(Obligation::Literal { expr, var });
                }
                self.check_literal_fits(&expr, answer);
                None
            }
        }
    }

    /// Type the numeric literal `expr` by the open variable `var` it meets,
    /// which then takes the literal's default where nothing else settles it.
    pub(super) fn literal_meets_var(&mut self, expr: &Expr, var: TypeId, default: TypeId) {
        let i = self.infer_holes.vars.index[&var];
        let root = self.infer_holes.vars.root(i);
        self.infer_holes.vars.add_literal_default(root, default);
        self.infer_holes.require(Obligation::Literal {
            expr: expr.clone(),
            var,
        });
    }

    /// Whether `ty` is a variable of this module nothing has settled yet.
    pub(super) fn is_open_var(&self, ty: TypeId) -> bool {
        self.tysys.type_table.borrow().is_infer_var(ty) && self.infer_holes.vars.is_unanswered(ty)
    }

    /// `ty` with its head settled where a site cannot go on without one: a
    /// variable only a literal met takes the literal's default here.
    pub(super) fn literal_head(&mut self, ty: TypeId) -> TypeId {
        let ty = self.apply_infer_holes(ty);
        if let Some(default) = self.infer_holes.vars.literal_default(ty) {
            self.answer_infer_var(ty, default);
            return self.apply_infer_holes(ty);
        }
        ty
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

    /// Whether `link`'s owner is answered, the link then settled for it, or
    /// for each element where the owner is a pack.
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
        for subject in self.tysys.bound_subjects(link.each_element, owner) {
            self.settle_projection_of(link, subject, written.as_deref());
        }
        true
    }

    /// `link` at the answered `owner`: the target is answered by the owner's
    /// projection or held to it. A projection that is not there, or not a
    /// tuple a pack takes, leaves the target to its own report.
    fn settle_projection_of(
        &mut self,
        link: &ProjectionLink,
        owner: TypeId,
        written: Option<&[TypeId]>,
    ) {
        let trait_args =
            written.and_then(|written| self.trait_args_at(link.trait_, owner, written));
        let forwarded = self
            .scope_type_param_ids()
            .owns(&self.tysys.type_table.borrow(), owner);
        let Some(answer) =
            self.bound_projection(owner, forwarded, link.trait_, trait_args, &link.assoc)
        else {
            return;
        };
        self.equate(answer, link.target);
        // Solving fills the holes and leaves the rest of the target as
        // written, which the answer must still match: `[i32, U]` against
        // `[bool, String]` settles `U` and misses at `i32`.
        let target = self.apply_infer_holes(link.target);
        if target == answer || says_nothing(target) || !self.decides_equality(target, answer) {
            return;
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
                Obligation::Bound { .. } | Obligation::Check(_) | Obligation::Literal { .. } => {
                    unreachable!("a settled variable decides a bound, a check or a literal")
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

    /// Substitute holes through every recorded fact that can carry a `TypeId`.
    /// Trait default-method bodies record into their own facts in
    /// `default_method_facts`, not the main `types`; sweep those too, or a
    /// hole minted in a default-method body leaks past reify.
    fn sweep_recorded_facts(&mut self, subst: &IndexMap<InferVarId, TypeId>) {
        if subst.is_empty() {
            return;
        }
        let mut tt = self.tysys.type_table.borrow_mut();
        Self::sweep_type_annotations(&mut tt, &mut self.sem.types, subst);
        for facts in self.sem.default_method_facts.values_mut() {
            Self::sweep_type_annotations(&mut tt, &mut facts.types, subst);
        }
    }

    /// Substitute `subst` through one `TypeAnnotations` fact bundle: the
    /// declaration-level maps, then every walk's body facts through
    /// [`Self::sweep_body_facts`].
    fn sweep_type_annotations(
        tt: &mut TypeTable,
        types: &mut TypeAnnotations,
        subst: &IndexMap<InferVarId, TypeId>,
    ) {
        sub_map(tt, &mut types.fn_return_types, subst);
        sub_map(tt, &mut types.function_task_returns, subst);
        sub_vec_map(tt, &mut types.fn_param_types, subst);
        sub_vec_map(tt, &mut types.fn_affine_params, subst);
        sub_vec_map(tt, &mut types.struct_field_types, subst);
        for i in types.impl_facts.values_mut() {
            sub_vec(tt, &mut i.trait_type_args, subst);
        }
        Self::sweep_body_facts(tt, &mut types.body, subst);
        for overlays in types.tuple_overlays.values_mut() {
            for overlay in overlays.iter_mut().flatten() {
                Self::sweep_body_facts(tt, overlay, subst);
            }
        }
    }

    /// Every body fact kind that can hold a `TypeId` has one substitution
    /// below; the module's own walk, each unrolled tuple `for-of` element and
    /// each default-argument walk hold the same kinds and sweep through the
    /// same list.
    fn sweep_body_facts(
        tt: &mut TypeTable,
        facts: &mut BodyFacts,
        subst: &IndexMap<InferVarId, TypeId>,
    ) {
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
            Self::sweep_body_facts(tt, overlay, subst);
        }
    }
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
    sub_monomorph(tt, &mut f.into_iter, subst);
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
