//! Deferred type-argument inference via inference holes.
//!
//! When a generic call's type parameter appears only in its return type and
//! the call site has no expected type yet (e.g. the receiver position of
//! `p.get().unwrap()`), the parameter cannot be inferred at the call. Instead
//! of erroring immediately, the elaborator mints an *inference hole* — a
//! reserved-index `TypeParam` standing in for the unknown — and lets the
//! holey type flow up the expression tree. When the hole later meets a
//! concrete expected type (the enclosing `.unwrap()`'s `i32` annotation), it
//! is solved. At the end of the module walk, solved holes are substituted
//! into every recorded fact; an unsolved hole raises a clean "cannot infer"
//! error and is pinned to `error` so nothing leaks to a later phase.
//!
//! Holes are `TypeParam`s with `index >= HOLE_INDEX_BASE`, so they reuse the
//! existing substitution / unification machinery and never collide with real
//! type parameters. A call is deferred only when its receiver and arguments
//! are hole-free, which guarantees the mangled names recorded for it carry no
//! hole — so a plain `TypeId` substitution sweep fully concretises every
//! recorded fact without any name re-mangling.

use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::infer::unify;
use super::types::TypeError;

/// Reserved start of the inference-hole `TypeParam` index space. Real type
/// parameters are indexed densely from 0, far below this, so the two never
/// overlap.
pub(super) const HOLE_INDEX_BASE: u32 = 0x8000_0000;

/// Per-module registry of inference holes and their (eventual) solutions.
#[derive(Default)]
pub(crate) struct InferHoleTable {
    /// Monotonic counter for fresh hole indices (added to [`HOLE_INDEX_BASE`]).
    next: u32,
    /// Hole `TypeId` → solution (`None` until solved).
    solutions: IndexMap<TypeId, Option<TypeId>>,
    /// Hole `TypeId` → diagnostic raised if it is never solved.
    diags: IndexMap<TypeId, (Span, String)>,
    /// Hole `TypeId` → the originating type parameter's `(name, trait-bound
    /// names, span)`. Once the hole is solved to a concrete type, the solution
    /// must satisfy those bounds — checked in [`Self::finalize_infer_holes`],
    /// since the bound check at the call site only saw the (unconstrained)
    /// hole. Without this a `get<T: Producer>()` solved to a non-`Producer`
    /// type would reach codegen and trap.
    bounds: IndexMap<TypeId, (String, Vec<String>, Span)>,
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
        bound_names: Vec<String>,
    ) -> TypeId {
        let index = HOLE_INDEX_BASE + self.infer_holes.next;
        self.infer_holes.next += 1;
        let name = format!("?{index}");
        let hole = self
            .tysys
            .type_table
            .borrow_mut()
            .make_type_param(name, index);
        self.infer_holes.solutions.insert(hole, None);
        self.infer_holes.diags.insert(hole, (span, message));
        if !bound_names.is_empty() {
            self.infer_holes
                .bounds
                .insert(hole, (param_name, bound_names, span));
        }
        hole
    }

    /// Whether `ty` contains any inference hole.
    pub(super) fn type_has_infer_hole(&self, ty: TypeId) -> bool {
        if self.infer_holes.is_empty() {
            return false;
        }
        self.tysys
            .type_table
            .borrow()
            .contains_infer_hole(ty, HOLE_INDEX_BASE)
    }

    /// Solve holes appearing in `holey` by unifying it against the concrete
    /// `expected`, recording each newly discovered binding. A binding is only
    /// taken when it is itself hole-free (a hole must resolve to a concrete
    /// type, never to another hole).
    pub(super) fn solve_infer_holes_against(&mut self, holey: TypeId, expected: TypeId) {
        if !self.type_has_infer_hole(holey) {
            return;
        }
        let mut bindings: IndexMap<TypeId, TypeId> = IndexMap::default();
        unify(&self.tysys.type_table, holey, expected, &mut bindings);
        if bindings.is_empty() {
            return;
        }
        let tt = self.tysys.type_table.borrow();
        for (hole, concrete) in bindings {
            if let Some(slot @ None) = self.infer_holes.solutions.get_mut(&hole)
                && !tt.contains_infer_hole(concrete, HOLE_INDEX_BASE)
            {
                *slot = Some(concrete);
            }
        }
    }

    /// Substitute already-solved holes into `ty` for in-flight concretisation
    /// (used before a recording site that embeds a mangled name derived from
    /// the type, where a later sweep could not fix the string).
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
            .substitute_type_params(ty, &subst)
    }

    /// Build the `hole-index → replacement` map. With `pin_unsolved`, unsolved
    /// holes map to `error` (used at finalize so nothing leaks); otherwise only
    /// solved holes are included.
    fn solved_hole_subst(&self, pin_unsolved: bool) -> IndexMap<u32, TypeId> {
        let tt = self.tysys.type_table.borrow();
        self.infer_holes
            .solutions
            .iter()
            .filter_map(|(hole, sol)| {
                let index = match tt.get(*hole) {
                    ResolvedType::TypeParam { index, .. } => *index,
                    _ => return None,
                };
                match sol {
                    Some(concrete) => Some((index, *concrete)),
                    None if pin_unsolved => Some((index, TypeTable::ERROR)),
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
        let mut seen: std::collections::HashSet<(Span, String)> = std::collections::HashSet::new();
        for (hole, (span, message)) in diags {
            let unsolved = self
                .infer_holes
                .solutions
                .get(&hole)
                .is_some_and(Option::is_none);
            // Several holes minted for one call share a message; emit it once.
            if unsolved && seen.insert((span, message.clone())) {
                let _ = self
                    .logger
                    .error(TypeError::CannotInferType { message, span });
            }
        }
        self.verify_solved_hole_bounds();
        let subst = self.solved_hole_subst(true);
        self.sweep_recorded_facts(&subst);
        self.infer_holes = InferHoleTable::default();
    }

    /// For each solved hole that came from a bounded type parameter, verify the
    /// solution satisfies the bound — the call-site check only saw the
    /// unconstrained hole. On success register the associated types so the
    /// monomorphizer can project them; on failure raise a clean trait-bound
    /// error (instead of trapping a later phase).
    fn verify_solved_hole_bounds(&mut self) {
        // Collect (solution, param_name, trait, span) for solved holes first so
        // the immutable `type_implements_trait` borrow is released before the
        // mutable register / diagnostic calls.
        let checks: Vec<(TypeId, String, String, Span)> = self
            .infer_holes
            .bounds
            .iter()
            .filter_map(|(hole, (param_name, trait_names, span))| {
                let solution = (*self.infer_holes.solutions.get(hole)?)?;
                Some(
                    trait_names
                        .iter()
                        .map(move |t| (solution, param_name.clone(), t.clone(), *span)),
                )
            })
            .flatten()
            .collect();

        for (solution, param_name, trait_name, span) in checks {
            // Same enforcement primitive the call-site type-arg loops use, so
            // the deferred re-check cannot drift from the eager check.
            self.enforce_single_bound(solution, &trait_name, &param_name, span);
        }
    }

    /// Substitute holes through every recorded fact map that can carry a
    /// `TypeId`. Because deferral only fires for hole-free receivers/args and
    /// enclosing calls are concretised before they record, no recorded
    /// *mangled name* embeds a hole — a `TypeId` substitution alone suffices.
    fn sweep_recorded_facts(&mut self, subst: &IndexMap<u32, TypeId>) {
        if subst.is_empty() {
            return;
        }
        let mut tt = self.tysys.type_table.borrow_mut();
        let types = &mut self.sem.types;

        let sub = |tt: &mut TypeTable, t: TypeId| tt.substitute_type_params(t, subst);
        let sub_vec = |tt: &mut TypeTable, v: &mut Vec<TypeId>| {
            for t in v.iter_mut() {
                *t = tt.substitute_type_params(*t, subst);
            }
        };

        for t in types.expression_types.values_mut() {
            *t = sub(&mut tt, *t);
        }
        for t in types.local_types.values_mut() {
            *t = sub(&mut tt, *t);
        }
        for t in types.let_annotated_types.values_mut() {
            *t = sub(&mut tt, *t);
        }
        for t in types.fn_return_types.values_mut() {
            *t = sub(&mut tt, *t);
        }
        for v in types.call_param_types.values_mut() {
            sub_vec(&mut tt, v);
        }
        for v in types.fn_param_types.values_mut() {
            sub_vec(&mut tt, v);
        }
        for v in types.struct_field_types.values_mut() {
            sub_vec(&mut tt, v);
        }
        for gi in types.generic_instantiations.values_mut() {
            sub_vec(&mut tt, &mut gi.type_args);
            gi.instance_type = sub(&mut tt, gi.instance_type);
        }
        for md in types.method_dispatch.values_mut() {
            md.return_type = sub(&mut tt, md.return_type);
            sub_vec(&mut tt, &mut md.method_type_args);
            if let Some(mi) = md.function_ref.monomorph_info.as_mut() {
                sub_vec(&mut tt, &mut mi.impl_type_args);
                sub_vec(&mut tt, &mut mi.method_type_args);
            }
        }
        for sd in types.static_method_dispatch.values_mut() {
            sub_vec(&mut tt, &mut sd.type_args);
            if let Some(mi) = sd.function_ref.monomorph_info.as_mut() {
                sub_vec(&mut tt, &mut mi.impl_type_args);
                sub_vec(&mut tt, &mut mi.method_type_args);
            }
        }
    }
}
