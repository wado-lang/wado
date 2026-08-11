//! Instantiating a polymorphic signature at a use site.
//!
//! A declaration is written in its own frame: `fn id<T>(x: T) -> T` names slot
//! 0 as a rigid `T`. A *use* of that declaration does not mention `T` at all —
//! it stands for whatever this call turns out to need. Instantiation is the
//! step that says so: each slot gets a fresh [`ResolvedType::InferVar`], and
//! the signature is rewritten into those variables before anything is checked
//! against it.
//!
//! Without this step the two roles collapse. `TypeParam` is interned by
//! `(name, index)`, so `fn f<T>`'s `T` and `fn g<T>`'s `T` are one `TypeId`,
//! and a check that meets a bare `T` cannot tell "the body that declares it"
//! (opaque, reject) from "a callee's slot about to be solved" (accept, record).
//! Every workaround for that ambiguity — comparing against the enclosing
//! scope's parameters, asking whether an argument could have pinned the slot,
//! consulting the bindings map for whether anything was inferred at all —
//! exists because the two share an id. Instantiating separates them at the
//! source instead of asking each site to guess.
//!
//! The variables minted here join the lifecycle [`super::infer_hole`] owns:
//! solved ones are substituted away, unsolved ones raise "cannot infer" and are
//! pinned to `error`, and neither survives elaboration.

use crate::compiler_host::CompilerHost;
use crate::hashmap::IndexMap;
use crate::tir::{ResolvedType, TypeId};
use crate::token::Span;

use super::Elaborator;

/// What is being instantiated, for the "cannot infer" diagnostic raised if a
/// slot is never solved.
pub(super) struct Instantiation<'a> {
    /// The declaration's kind, e.g. `"function"` or `"method"`.
    pub(super) kind: &'a str,
    /// The declaration's name.
    pub(super) name: &'a str,
    /// Where the use site is.
    pub(super) span: Span,
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
    diags: Vec<Option<(Span, String)>>,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Instantiate `slots` — a declaration's type parameters, in order — with
    /// one fresh inference variable each.
    ///
    /// `mint[i]` declines the `i`th slot, leaving it rigid. A parameter whose
    /// bound already fixes its shape (`<F: fn(...)>`) is constrained
    /// structurally rather than by call-site inference, so minting a variable
    /// for it would only produce an unsolvable one.
    ///
    /// A type *pack* (`..T`) is likewise left rigid. A variable stands for one
    /// type; a pack stands for a list of them, and the unifier splices it by
    /// recognising `TypePack` inside the expected tuple. Rewriting `[..T]` to
    /// `[?0]` would hide the shape that arm matches on and the pack would
    /// never bind. Instantiating a pack needs a pack-shaped variable, which
    /// does not exist yet.
    pub(super) fn instantiate(
        &mut self,
        slots: &[TypeId],
        mint: &[bool],
        of: &Instantiation<'_>,
    ) -> Instantiated {
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
            let Some((index, name)) = named_slot.filter(|_| mint.get(i).copied().unwrap_or(true))
            else {
                vars.push(slot);
                diags.push(None);
                continue;
            };
            let var = self.mint_infer_var();
            subst.insert(index, var);
            vars.push(var);
            diags.push(Some((
                of.span,
                format!(
                    "cannot infer type parameter `{name}` of {} `{}`; \
                     add a turbofish (`{}::<...>()`) or a type annotation",
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

    /// Attach each slot's "cannot infer" diagnostic to the variable still
    /// standing in for it, so an unsolved one is reported at finalize.
    pub(super) fn blame_unsolved(&mut self, inst: &Instantiated, solved: &[TypeId]) {
        for (i, &var) in inst.vars.iter().enumerate() {
            if solved.get(i) != Some(&var) {
                continue;
            }
            if let Some((span, message)) = inst.diags[i].clone() {
                self.attach_infer_var_diag(var, span, message);
            }
        }
    }
}
