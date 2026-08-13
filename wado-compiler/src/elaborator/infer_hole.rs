//! Deferred type-argument inference via inference holes.
//!
//! When a generic call's type parameter appears only in its return type and the
//! call site has no expected type yet (e.g. `p.get()` in `p.get().unwrap()`),
//! the elaborator mints an *inference hole* and lets the holey type flow up
//! until a concrete expected type solves it. The module-end sweep substitutes
//! solved holes into recorded facts; an unsolved hole raises "cannot infer" and
//! is pinned to `error`.
//!
//! A hole is a [`ResolvedType::InferVar`] — a *flexible* variable, distinct
//! from the *rigid* `TypeParam` it stands in for. A name already mangled from
//! one is rebuilt from the swept type arguments, not patched.

use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::tir::{InferVarId, ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::infer::unify;
use super::types::TypeError;

/// Per-module registry of inference holes and their (eventual) solutions.
#[derive(Default)]
pub(crate) struct InferHoleTable {
    /// Hole `TypeId` → solution (`None` until solved).
    solutions: IndexMap<TypeId, Option<TypeId>>,
    /// Hole `TypeId` → what it says if it is never solved.
    diags: IndexMap<TypeId, Blame>,
    /// Hole `TypeId` → originating parameter's `(name, bounds, span)`,
    /// re-verified against the solution in [`Self::finalize_infer_holes`] (the
    /// call-site check only saw the unconstrained hole).
    ///
    /// Each bound is the declaration its own reference site names, paired with
    /// the spelling that site wrote for the diagnostic. Storing the spelling
    /// alone loses the site, and the re-check then has no identity to enforce
    /// against.
    bounds: IndexMap<TypeId, (String, Vec<DeclaredBound>, Span)>,
}

/// A trait bound a slot declared: the declaration its reference site names,
/// and the spelling that site wrote.
#[derive(Clone, Debug)]
pub(crate) struct DeclaredBound {
    pub(crate) decl: crate::defs::DefId,
    pub(crate) written: String,
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
            .map(|p| format!("`{p}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("cannot infer type parameter {named} {}", self.owner)
    }
}

impl InferHoleTable {
    pub(super) fn is_empty(&self) -> bool {
        self.solutions.is_empty()
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
        let hole = self.mint_infer_var();
        self.attach_infer_var_diag(hole, span, message);
        if !bounds.is_empty() {
            self.infer_holes
                .bounds
                .insert(hole, (param_name, bounds, span));
        }
        hole
    }

    /// The bounds `param` declares, each as the declaration its own site names
    /// plus the spelling that site wrote.
    ///
    /// A bound whose site reaches no declaration is dropped: it is diagnosed
    /// where it was written, and there is nothing to enforce a solution
    /// against.
    pub(super) fn declared_bounds(&self, param: &crate::ast::GenericParam) -> Vec<DeclaredBound> {
        param
            .bounds
            .iter()
            .filter(|b| b.fn_signature.is_none())
            .filter_map(|b| {
                Some(DeclaredBound {
                    decl: self.bound_trait_def(b.id)?,
                    written: b.name.clone(),
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
        let var = InferVarId(self.infer_holes.solutions.len() as u32);
        let hole = self.tysys.type_table.borrow_mut().make_infer_var(var);
        assert!(
            self.infer_holes.solutions.insert(hole, None).is_none(),
            "inference variable {var} minted twice"
        );
        hole
    }

    /// Remember the trait bounds `var`'s slot declared, re-verified against
    /// the solution in [`Self::finalize_infer_holes`].
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
        self.infer_holes
            .bounds
            .entry(var)
            .or_insert((param_name, bounds, span));
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
        variant_info: &super::types::VariantInfo,
        span: Span,
    ) -> TypeId {
        let arity = variant_info.type_param_type_ids.len();
        if arity == 0 {
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
        let message = format!(
            "cannot infer type parameter of variant `{variant_name}`; add a turbofish (`{variant_name}::<...>::…`) or a type annotation"
        );
        let holes: Vec<TypeId> = (0..arity)
            .map(|_| {
                self.mint_infer_hole(span, message.clone(), variant_name.to_string(), Vec::new())
            })
            .collect();
        {
            let def = self
                .tysys
                .type_table
                .borrow_mut()
                .decl_named_in(&variant_info.name, &variant_info.module_source)
                .expect("the declaration this type names exists");
            self.tysys
                .type_table
                .borrow_mut()
                .make_generic_instance(def, holes)
        }
    }

    pub(super) fn type_has_infer_hole(&self, ty: TypeId) -> bool {
        if self.infer_holes.is_empty() {
            return false;
        }
        self.tysys.type_table.borrow().contains_infer_var(ty)
    }

    /// Whether a deferred inference hole may be solved against `expected`.
    ///
    /// `expected` must be hole-free and mention only outer-scope type parameters
    /// (the enclosing impl / fn generics). Pinning a hole to a callee's *own*
    /// method type parameter — still being inferred at this call site — would
    /// fuse the hole to a dangling id instead of deferring it to a real sink.
    pub(super) fn hole_pinnable_against(&self, expected: TypeId) -> bool {
        if self.type_has_infer_hole(expected) {
            return false;
        }
        let scope: Vec<TypeId> = self
            .annotate_ctx
            .trait_ctx
            .type_params
            .values()
            .map(|&(_, tid)| tid)
            .collect();
        self.tysys
            .type_table
            .borrow()
            .type_params_all_in(expected, &scope)
    }

    /// Record `answer` as the solution of `var` — the direct form of
    /// [`Self::solve_infer_holes_against`], for a use site whose own solver
    /// already determined the type argument. Keeps the first answer, matching
    /// the unifier's `or_insert` policy, and refuses an answer that is itself
    /// still a variable.
    pub(super) fn solve_infer_var(&mut self, var: TypeId, answer: TypeId) {
        if !self.is_usable_answer(answer) {
            return;
        }
        if let Some(slot @ None) = self.infer_holes.solutions.get_mut(&var) {
            *slot = Some(answer);
        }
    }

    /// Whether `answer` can stand as a variable's solution.
    ///
    /// Another variable cannot: a variable resolves to a type, not to a
    /// deferral. Neither can `never`, `unknown` or `error` — each is a type a
    /// check accepts *anywhere*, so taking one as the answer would fix the
    /// variable to it and measure every later candidate against it. The
    /// element type of `[panic(), 1]` is not `!`.
    fn is_usable_answer(&self, answer: TypeId) -> bool {
        answer != TypeTable::NEVER
            && answer != TypeTable::UNKNOWN
            && answer != TypeTable::ERROR
            && !self.tysys.type_table.borrow().contains_infer_var(answer)
    }

    /// Solve holes in `holey` by unifying against `expected`. A binding is taken
    /// only when hole-free — a hole must resolve to a concrete type, not another.
    pub(super) fn solve_infer_holes_against(&mut self, holey: TypeId, expected: TypeId) {
        if !self.type_has_infer_hole(holey) {
            return;
        }
        let mut bindings: IndexMap<TypeId, TypeId> = IndexMap::default();
        unify(&self.tysys.type_table, holey, expected, &mut bindings);
        if bindings.is_empty() {
            return;
        }
        let usable: Vec<(TypeId, TypeId)> = bindings
            .into_iter()
            .filter(|&(_, concrete)| self.is_usable_answer(concrete))
            .collect();
        for (hole, concrete) in usable {
            if let Some(slot @ None) = self.infer_holes.solutions.get_mut(&hole) {
                *slot = Some(concrete);
            }
        }
    }

    /// Substitute solved holes into `ty` now — used before a site that records a
    /// mangled name derived from the type, which a later sweep could not fix.
    pub(super) fn apply_infer_holes(&mut self, ty: TypeId) -> TypeId {
        if !self.type_has_infer_hole(ty) {
            return ty;
        }
        let subst = self.solved_hole_subst(false);
        if subst.is_empty() {
            return ty;
        }
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_infer_vars(ty, &subst)
    }

    /// Build the `variable → replacement` map. With `pin_unsolved`, unsolved
    /// variables map to `error` (used at finalize so nothing leaks); otherwise
    /// only solved ones are included.
    fn solved_hole_subst(&self, pin_unsolved: bool) -> IndexMap<InferVarId, TypeId> {
        let tt = self.tysys.type_table.borrow();
        self.infer_holes
            .solutions
            .iter()
            .filter_map(|(hole, sol)| {
                let ResolvedType::InferVar(var) = tt.get(*hole) else {
                    panic!("infer-hole table holds a non-variable type");
                };
                match sol {
                    Some(concrete) => Some((*var, *concrete)),
                    None if pin_unsolved => Some((*var, TypeTable::ERROR)),
                    None => None,
                }
            })
            .collect()
    }

    /// End-of-module finalize: raise diagnostics for unsolved holes and
    /// substitute every hole (solved → concrete, unsolved → `error`) into all
    /// recorded facts that can carry one.
    pub(super) fn finalize_infer_holes(&mut self) {
        if self.infer_holes.is_empty() {
            return;
        }
        let diags = std::mem::take(&mut self.infer_holes.diags);
        // Group by what the variables belong to, in mint order, so one use
        // site's unsolved slots become one message naming them all.
        let mut groups: IndexMap<(Span, String), Vec<Option<String>>> = IndexMap::default();
        let mut owners: IndexMap<(Span, String), Blame> = IndexMap::default();
        for (hole, blame) in diags {
            if !self
                .infer_holes
                .solutions
                .get(&hole)
                .is_some_and(Option::is_none)
            {
                continue;
            }
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
            let _ = self.logger.error(TypeError::CannotInferType {
                message: blame.message(&unique),
                span: blame.span,
            });
        }
        self.verify_solved_hole_bounds();
        let subst = self.solved_hole_subst(true);
        self.sweep_recorded_facts(&subst);
        self.infer_holes = InferHoleTable::default();
    }

    /// Verify each solved bounded hole satisfies its bound (the call-site check
    /// only saw the unconstrained hole), registering associated types on success
    /// or raising a clean trait-bound error on failure.
    fn verify_solved_hole_bounds(&mut self) {
        // Collect first so the `type_implements_trait` borrow is released before
        // the mutable enforce calls.
        let tt = self.tysys.type_table.borrow();
        let checks: Vec<(TypeId, String, DeclaredBound, Span)> = self
            .infer_holes
            .bounds
            .iter()
            .filter_map(|(hole, (param_name, bounds, span))| {
                let solution = (*self.infer_holes.solutions.get(hole)?)?;
                // A still-parametric solution (forwarded param, or another hole)
                // is checked when its owner is monomorphized; re-checking here,
                // after the bound's scope closed, would spuriously fail.
                if tt.contains_type_param(solution) {
                    return None;
                }
                Some(
                    bounds
                        .iter()
                        .map(move |b| (solution, param_name.clone(), b.clone(), *span)),
                )
            })
            .flatten()
            .collect();
        drop(tt);

        for (solution, param_name, bound, span) in checks {
            self.enforce_single_bound(
                solution,
                &bound.written,
                Some(bound.decl),
                &param_name,
                span,
            );
        }
    }

    /// Substitute holes through every recorded fact that can carry a `TypeId`.
    /// Trait default-method bodies record into a separate synthetic semantics
    /// stashed in `default_method_semantics`, not the main `types`; sweep those
    /// too, or a hole minted in a default-method body leaks past reify.
    fn sweep_recorded_facts(&mut self, subst: &IndexMap<InferVarId, TypeId>) {
        if subst.is_empty() {
            return;
        }
        let mut tt = self.tysys.type_table.borrow_mut();
        Self::sweep_type_annotations(&mut tt, &mut self.sem.types, subst);
        for sem in self.sem.default_method_semantics.values_mut() {
            Self::sweep_type_annotations(&mut tt, &mut sem.types, subst);
        }
    }

    /// Substitute `subst` through one `TypeAnnotations` fact bundle.
    ///
    /// Every fact kind that can hold a `TypeId` has one substitution below, so
    /// the per-iteration overlays — which hold the same kinds — sweep by
    /// calling the same ones rather than by a second hand-kept list that
    /// falls behind this one.
    fn sweep_type_annotations(
        tt: &mut TypeTable,
        types: &mut super::sem::TypeAnnotations,
        subst: &IndexMap<InferVarId, TypeId>,
    ) {
        sub_map(tt, &mut types.expression_types, subst);
        sub_map(tt, &mut types.local_types, subst);
        sub_map(tt, &mut types.let_annotated_types, subst);
        sub_map(tt, &mut types.fn_return_types, subst);
        sub_map(tt, &mut types.function_task_returns, subst);
        sub_vec_map(tt, &mut types.call_param_types, subst);
        sub_vec_map(tt, &mut types.fn_param_types, subst);
        sub_vec_map(tt, &mut types.struct_field_types, subst);
        for gi in types.generic_instantiations.values_mut() {
            sub_generic_instantiation(tt, gi, subst);
        }
        for md in types.method_dispatch.values_mut() {
            sub_method_dispatch(tt, md, subst);
        }
        for sd in types.static_method_dispatch.values_mut() {
            sub_static_dispatch(tt, sd, subst);
        }
        for c in types.coercions.values_mut() {
            c.target_type = sub(tt, c.target_type, subst);
        }
        for od in types
            .operator_dispatch
            .values_mut()
            .chain(types.index_assign_dispatch.values_mut())
        {
            sub_operator_dispatch(tt, od, subst);
        }
        for f in types.for_of_iterator.values_mut() {
            sub_for_of(tt, f, subst);
        }
        for cc in types.closure_captures.values_mut() {
            sub_closure_captures(tt, cc, subst);
        }
        for h in types.handler_bindings.values_mut() {
            h.handler_type = sub(tt, h.handler_type, subst);
            for e in &mut h.effects {
                sub_vec(tt, &mut e.trait_type_args, subst);
            }
        }
        for i in types.impl_facts.values_mut() {
            sub_vec(tt, &mut i.trait_type_args, subst);
        }
        for sc in types.sequence_coercions.values_mut() {
            sub_sequence_coercion(tt, sc, subst);
        }
        for kv in types.key_value_coercions.values_mut() {
            sub_key_value_coercion(tt, kv, subst);
        }
        for overlays in types.tuple_overlays.values_mut() {
            for overlay in overlays.iter_mut().flatten() {
                Self::sweep_element_overlay(tt, overlay, subst);
            }
        }
    }

    /// [`Self::sweep_type_annotations`] for a per-iteration overlay, which
    /// holds the same fact kinds for the nodes one unrolled tuple `for-of`
    /// iteration rebinds.
    fn sweep_element_overlay(
        tt: &mut TypeTable,
        overlay: &mut super::sem::types::ElementOverlay,
        subst: &IndexMap<InferVarId, TypeId>,
    ) {
        sub_map(tt, &mut overlay.expression_types, subst);
        sub_map(tt, &mut overlay.local_types, subst);
        sub_map(tt, &mut overlay.let_annotated_types, subst);
        sub_vec_map(tt, &mut overlay.call_param_types, subst);
        for gi in overlay.generic_instantiations.values_mut() {
            sub_generic_instantiation(tt, gi, subst);
        }
        for md in overlay.method_dispatch.values_mut() {
            sub_method_dispatch(tt, md, subst);
        }
        for sd in overlay.static_method_dispatch.values_mut() {
            sub_static_dispatch(tt, sd, subst);
        }
        for c in overlay.coercions.values_mut() {
            c.target_type = sub(tt, c.target_type, subst);
        }
        for od in overlay
            .operator_dispatch
            .values_mut()
            .chain(overlay.index_assign_dispatch.values_mut())
        {
            sub_operator_dispatch(tt, od, subst);
        }
        for f in overlay.for_of_iterator.values_mut() {
            sub_for_of(tt, f, subst);
        }
        for cc in overlay.closure_captures.values_mut() {
            sub_closure_captures(tt, cc, subst);
        }
        for sc in overlay.sequence_coercions.values_mut() {
            sub_sequence_coercion(tt, sc, subst);
        }
        for kv in overlay.key_value_coercions.values_mut() {
            sub_key_value_coercion(tt, kv, subst);
        }
    }
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
    m: &mut IndexMap<crate::ast::AstId, TypeId>,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    for t in m.values_mut() {
        *t = sub(tt, *t, subst);
    }
}

fn sub_vec_map(
    tt: &mut TypeTable,
    m: &mut IndexMap<crate::ast::AstId, Vec<TypeId>>,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    for v in m.values_mut() {
        sub_vec(tt, v, subst);
    }
}

fn sub_monomorph(
    tt: &mut TypeTable,
    f: &mut crate::tir::FunctionRef,
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
    gi: &mut super::sem::types::GenericInstantiation,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    sub_vec(tt, &mut gi.type_args, subst);
    gi.instance_type = sub(tt, gi.instance_type, subst);
    if let Some(name) = gi.mangled_name.as_mut()
        && let ResolvedType::GenericInstance { def, type_args } = tt.get(gi.instance_type).clone()
        && let base = tt.def_name(def).to_string()
    {
        let arg_names: Vec<String> = type_args.iter().map(|&t| tt.type_name(t)).collect();
        *name = crate::name::mangle_generic_name(&base, &arg_names);
    }
}

fn sub_method_dispatch(
    tt: &mut TypeTable,
    md: &mut super::sem::types::MethodDispatch,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    md.return_type = sub(tt, md.return_type, subst);
    sub_vec(tt, &mut md.method_type_args, subst);
    sub_monomorph(tt, &mut md.function_ref, subst);
}

fn sub_static_dispatch(
    tt: &mut TypeTable,
    sd: &mut super::sem::types::StaticMethodDispatch,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    sub_vec(tt, &mut sd.type_args, subst);
    sub_vec(tt, &mut sd.param_types, subst);
    sub_monomorph(tt, &mut sd.function_ref, subst);
}

fn sub_operator_dispatch(
    tt: &mut TypeTable,
    od: &mut super::sem::types::OperatorDispatch,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    od.return_type = sub(tt, od.return_type, subst);
    sub_monomorph(tt, &mut od.function_ref, subst);
}

fn sub_for_of(
    tt: &mut TypeTable,
    f: &mut super::sem::types::ForOfIteratorInfo,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    f.item_type = sub(tt, f.item_type, subst);
    f.iter_type = sub(tt, f.iter_type, subst);
    sub_monomorph(tt, &mut f.into_iter, subst);
    sub_monomorph(tt, &mut f.next, subst);
}

fn sub_closure_captures(
    tt: &mut TypeTable,
    cc: &mut super::sem::types::ClosureCaptureInfo,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    for c in &mut cc.captures {
        c.type_id = sub(tt, c.type_id, subst);
    }
    for m in &mut cc.mut_captures {
        m.inner_type = sub(tt, m.inner_type, subst);
        m.ref_type = sub(tt, m.ref_type, subst);
    }
}

fn sub_sequence_coercion(
    tt: &mut TypeTable,
    sc: &mut super::sem::types::SequenceCoercionFacts,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    sc.builder_type = sub(tt, sc.builder_type, subst);
    sc.element_type = sub(tt, sc.element_type, subst);
    sc.output_type = sub(tt, sc.output_type, subst);
    sc.newtype_cast_to = sc.newtype_cast_to.map(|t| sub(tt, t, subst));
    sub_vec(tt, &mut sc.type_arg_ids, subst);
    sc.remangle(tt);
}

fn sub_key_value_coercion(
    tt: &mut TypeTable,
    kv: &mut super::sem::types::KeyValueCoercionFacts,
    subst: &IndexMap<InferVarId, TypeId>,
) {
    kv.builder_type = sub(tt, kv.builder_type, subst);
    kv.value_type = sub(tt, kv.value_type, subst);
    kv.target_type = sub(tt, kv.target_type, subst);
    sub_vec(tt, &mut kv.type_arg_ids, subst);
    kv.remangle(tt);
}
