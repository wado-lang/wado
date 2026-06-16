//! Deferred type-argument inference via inference holes.
//!
//! When a generic call's type parameter appears only in its return type and the
//! call site has no expected type yet (e.g. `p.get()` in `p.get().unwrap()`),
//! the elaborator mints an *inference hole* and lets the holey type flow up
//! until a concrete expected type solves it. The module-end sweep substitutes
//! solved holes into recorded facts; an unsolved hole raises "cannot infer" and
//! is pinned to `error`.
//!
//! Holes are `TypeParam`s with `index >= HOLE_INDEX_BASE`, reusing the
//! unification/substitution machinery without colliding with real parameters.
//! Deferral fires only for hole-free receivers/args, so no recorded mangled
//! name embeds a hole and a plain `TypeId` sweep suffices.

use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use super::infer::unify;
use super::types::TypeError;

/// Reserved start of the inference-hole `TypeParam` index space, far above the
/// dense-from-0 real type parameters so the two never overlap.
pub(super) const HOLE_INDEX_BASE: u32 = 0x8000_0000;

/// Per-module registry of inference holes and their (eventual) solutions.
#[derive(Default)]
pub(crate) struct InferHoleTable {
    /// Hole `TypeId` → solution (`None` until solved).
    solutions: IndexMap<TypeId, Option<TypeId>>,
    /// Hole `TypeId` → diagnostic raised if it is never solved.
    diags: IndexMap<TypeId, (Span, String)>,
    /// Hole `TypeId` → originating parameter's `(name, trait-bound names, span)`,
    /// re-verified against the solution in [`Self::finalize_infer_holes`] (the
    /// call-site check only saw the unconstrained hole).
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
        // Holes are only appended, so the next dense index is the current count.
        let index = HOLE_INDEX_BASE + self.infer_holes.solutions.len() as u32;
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

    pub(super) fn type_has_infer_hole(&self, ty: TypeId) -> bool {
        if self.infer_holes.is_empty() {
            return false;
        }
        self.tysys
            .type_table
            .borrow()
            .contains_infer_hole(ty, HOLE_INDEX_BASE)
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
        let tt = self.tysys.type_table.borrow();
        for (hole, concrete) in bindings {
            if let Some(slot @ None) = self.infer_holes.solutions.get_mut(&hole)
                && !tt.contains_infer_hole(concrete, HOLE_INDEX_BASE)
            {
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

    /// Verify each solved bounded hole satisfies its bound (the call-site check
    /// only saw the unconstrained hole), registering associated types on success
    /// or raising a clean trait-bound error on failure.
    fn verify_solved_hole_bounds(&mut self) {
        // Collect first so the `type_implements_trait` borrow is released before
        // the mutable enforce calls.
        let tt = self.tysys.type_table.borrow();
        let checks: Vec<(TypeId, String, String, Span)> = self
            .infer_holes
            .bounds
            .iter()
            .filter_map(|(hole, (param_name, trait_names, span))| {
                let solution = (*self.infer_holes.solutions.get(hole)?)?;
                // A still-parametric solution (forwarded param, or another hole)
                // is checked when its owner is monomorphized; re-checking here,
                // after the bound's scope closed, would spuriously fail.
                if tt.contains_type_param(solution) {
                    return None;
                }
                Some(
                    trait_names
                        .iter()
                        .map(move |t| (solution, param_name.clone(), t.clone(), *span)),
                )
            })
            .flatten()
            .collect();
        drop(tt);

        for (solution, param_name, trait_name, span) in checks {
            self.enforce_single_bound(solution, &trait_name, &param_name, span);
        }
    }

    /// Substitute holes through every recorded fact that can carry a `TypeId`.
    /// Trait default-method bodies record into a separate synthetic semantics
    /// stashed in `default_method_semantics`, not the main `types`; sweep those
    /// too, or a hole minted in a default-method body leaks past reify.
    fn sweep_recorded_facts(&mut self, subst: &IndexMap<u32, TypeId>) {
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
    fn sweep_type_annotations(
        tt: &mut TypeTable,
        types: &mut super::sem::TypeAnnotations,
        subst: &IndexMap<u32, TypeId>,
    ) {
        let sub = |tt: &mut TypeTable, t: TypeId| tt.substitute_type_params(t, subst);
        let sub_vec = |tt: &mut TypeTable, v: &mut Vec<TypeId>| {
            for t in v.iter_mut() {
                *t = tt.substitute_type_params(*t, subst);
            }
        };

        for t in types.expression_types.values_mut() {
            *t = sub(tt, *t);
        }
        for t in types.local_types.values_mut() {
            *t = sub(tt, *t);
        }
        for t in types.let_annotated_types.values_mut() {
            *t = sub(tt, *t);
        }
        for t in types.fn_return_types.values_mut() {
            *t = sub(tt, *t);
        }
        for v in types.call_param_types.values_mut() {
            sub_vec(tt, v);
        }
        for v in types.fn_param_types.values_mut() {
            sub_vec(tt, v);
        }
        for v in types.struct_field_types.values_mut() {
            sub_vec(tt, v);
        }
        for gi in types.generic_instantiations.values_mut() {
            sub_vec(tt, &mut gi.type_args);
            gi.instance_type = sub(tt, gi.instance_type);
        }
        for md in types.method_dispatch.values_mut() {
            md.return_type = sub(tt, md.return_type);
            sub_vec(tt, &mut md.method_type_args);
            if let Some(mi) = md.function_ref.monomorph_info.as_mut() {
                sub_vec(tt, &mut mi.impl_type_args);
                sub_vec(tt, &mut mi.method_type_args);
            }
        }
        for sd in types.static_method_dispatch.values_mut() {
            sub_vec(tt, &mut sd.type_args);
            if let Some(mi) = sd.function_ref.monomorph_info.as_mut() {
                sub_vec(tt, &mut mi.impl_type_args);
                sub_vec(tt, &mut mi.method_type_args);
            }
        }
    }
}
