//! Instantiating a polymorphic signature at a use site. A declaration is written
//! in its own frame — `fn id<T>(x: T) -> T` names slot 0 a rigid `T` — but a use
//! of it stands for whatever this call needs, so each slot gets a fresh
//! [`ResolvedType::InferVar`] and the signature is rewritten into those before
//! anything is checked. They then join the lifecycle [`super::infer_hole`] owns.

use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use super::Elaborator;
use crate::ast::GenericParam;

/// What is being instantiated, for the "cannot infer" diagnostic raised if a
/// slot is never solved.
pub(super) struct Instantiation<'a> {
    /// The declaration's kind, e.g. `"function"` or `"method"`.
    pub(super) kind: &'a str,
    /// The declaration's name.
    pub(super) name: &'a str,
    /// Where the use site is.
    pub(super) span: Span,
    /// What the use site's turbofish names, in slot order:
    /// [`TypeTable::UNKNOWN`] where it wrote `_` or stopped short, and empty
    /// where it wrote no turbofish at all.
    ///
    /// A field rather than a later call, so a site cannot instantiate without
    /// saying what it already knows. An argument is checked against these
    /// slots, and a turbofish applied after that walk is applied too late:
    /// `go::<String>(|a| a.len())` would resolve the closure body against a
    /// variable nothing has answered.
    pub(super) type_args: &'a [TypeId],
}

/// A declaration's slots rewritten into one use site's variables.
pub(super) struct Instantiated {
    /// What each slot became, in declaration order: a fresh variable, or the
    /// slot itself where instantiation was declined. Feed this to
    /// `InferCtx::new` so `solve` answers positionally.
    pub(super) vars: Vec<TypeId>,
    /// Rewrites a type written in the declaration's frame into this use site's
    /// variables. Apply with `TypeTable::substitute_type_params`.
    subst: IndexMap<u32, TypeId>,
    /// Per-slot "cannot infer" diagnostic, attached by
    /// [`Elaborator::record_instantiation`] to the slots still unsolved when
    /// the use site commits. Held rather than attached at mint time because a
    /// site may instantiate speculatively — inference runs twice for a partial
    /// turbofish — and a discarded instantiation must report nothing.
    ///
    /// `None` marks a slot this instantiation will never have to solve: one
    /// left rigid, or one the turbofish already named. Neither can go
    /// unanswered, so neither carries a blame or a bound to re-check — the
    /// turbofish's own types are bounds-checked where they are read.
    diags: Vec<Option<(Span, String, String)>>,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Instantiate `slots` — a declaration's type parameters, in order — with
    /// one fresh inference variable each.
    ///
    /// A type *pack* (`..T`) is left rigid. A variable stands for one type; a
    /// pack stands for a list of them, and the unifier splices it by
    /// recognising `TypePack` inside the expected tuple. Rewriting `[..T]` to
    /// `[?0]` would hide the shape that arm matches on and the pack would
    /// never bind. Instantiating a pack needs a pack-shaped variable, which
    /// does not exist yet.
    ///
    /// A slot [`Instantiation::type_args`] names is solved to that type as it
    /// is minted, so the variable is a hole only where the site has no answer
    /// yet.
    pub(super) fn instantiate(&mut self, slots: &[TypeId], of: &Instantiation<'_>) -> Instantiated {
        let mut vars = Vec::with_capacity(slots.len());
        let mut diags = Vec::with_capacity(slots.len());
        let mut subst = IndexMap::default();
        for (i, &slot) in slots.iter().enumerate() {
            let named_slot = {
                let tt = self.tysys.type_table.borrow();
                match tt.get(slot) {
                    ResolvedType::TypeParam { index, name } => Some((*index, name.clone())),
                    _ => None,
                }
            };
            let Some((index, name)) = named_slot else {
                vars.push(slot);
                diags.push(None);
                continue;
            };
            let var = self.mint_infer_var_named(&name);
            subst.insert(index, var);
            vars.push(var);
            // A slot the turbofish names is answered before the first argument
            // is checked against it, so what the source wrote is what an
            // argument — a closure body above all — meets.
            if let Some(&named) = of.type_args.get(i)
                && named != TypeTable::UNKNOWN
            {
                self.solve_infer_var(var, named);
                diags.push(None);
                continue;
            }
            // The parameter and the rest of the sentence stay apart so
            // `finalize_infer_holes` can name every unsolved slot of this use
            // site in one message.
            diags.push(Some((
                of.span,
                name,
                format!(
                    "of {} `{}`; add a turbofish (`{}::<...>()`) or a type annotation",
                    of.kind, of.name, of.name
                ),
            )));
        }
        Instantiated { vars, subst, diags }
    }

    /// Rewrite a type written in a declaration's frame into `inst`'s variables.
    pub(super) fn instantiate_type(&mut self, ty: TypeId, inst: &Instantiated) -> TypeId {
        if inst.subst.is_empty() {
            return ty;
        }
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(ty, &inst.subst)
    }

    /// [`Self::instantiate_type`] over a signature's parameter list.
    pub(super) fn instantiate_types(&mut self, tys: &[TypeId], inst: &Instantiated) -> Vec<TypeId> {
        if inst.subst.is_empty() {
            return tys.to_vec();
        }
        tys.iter()
            .map(|&t| self.instantiate_type(t, inst))
            .collect()
    }

    /// Record what the use site's solver determined for each slot, so the
    /// module-end sweep substitutes the variables out of every recorded fact.
    ///
    /// A slot the solver left as its own variable stays unsolved. Reporting it
    /// is [`Self::blame_unsolved`]'s job, kept separate because a site may
    /// still resolve the slot after solving — the free-function path defers to
    /// `defer_or_report_uninferred_fn_type_args` — and only the site that
    /// gives up should raise the diagnostic.
    pub(super) fn record_instantiation(&mut self, inst: &Instantiated, solved: &[TypeId]) {
        for (&var, &answer) in inst.vars.iter().zip(solved.iter()) {
            if var != answer {
                self.solve_infer_var(var, answer);
            }
        }
    }

    /// Settle the variables an argument walk used back onto the slots they
    /// stand for, substituting the answers through `args`.
    ///
    /// [`Self::solve_infer_var`] keeps the first answer, so a slot the
    /// arguments pinned stays pinned and one they left open returns to the
    /// declaration's parameter. Type-argument inference then sees the rigid
    /// signature it would have seen had the arguments never been instantiated
    /// against.
    pub(super) fn settle_onto_slots(
        &mut self,
        inst: &Instantiated,
        slots: &[TypeId],
        args: &mut [TypeId],
    ) {
        for (&var, &slot) in inst.vars.iter().zip(slots.iter()) {
            self.solve_infer_var(var, slot);
        }
        for arg in args {
            *arg = self.apply_infer_holes(*arg);
        }
    }

    /// Carry each slot's declared trait bounds onto the variable standing in
    /// for it, so [`Self::finalize_infer_holes`] re-checks whatever solved it.
    ///
    /// A call site only ever meets the unconstrained variable, so this is the
    /// single point at which `get<T: Producer>()` deferred to a later sink can
    /// still be told that `String` is not a `Producer`. Nothing is attached to
    /// a slot instantiation declined, which is rigid and never solved.
    pub(super) fn record_slot_bounds(
        &mut self,
        inst: &Instantiated,
        params: &[GenericParam],
        span: Span,
    ) {
        for ((&var, param), diag) in inst.vars.iter().zip(params.iter()).zip(inst.diags.iter()) {
            // `diags` is `None` exactly where instantiation was declined, and
            // that slot is rigid: it is never solved, so a bound recorded
            // against it could never be re-checked.
            if diag.is_none() {
                continue;
            }
            let bounds = self.declared_bounds(param);
            self.attach_infer_var_bounds(var, param.name.clone(), bounds, span);
        }
    }

    /// Attach each slot's "cannot infer" diagnostic to the variable still
    /// standing in for it, so an unsolved one is reported at finalize.
    pub(super) fn blame_unsolved(&mut self, inst: &Instantiated, solved: &[TypeId]) {
        for (i, &var) in inst.vars.iter().enumerate() {
            if solved.get(i) != Some(&var) {
                continue;
            }
            if let Some((span, param, owner)) = inst.diags[i].clone() {
                self.attach_infer_var_blame(var, span, param, owner);
            }
        }
    }
}
