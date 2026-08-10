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
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Instantiate `slots` — a declaration's type parameters, in order — with
    /// one fresh inference variable each.
    ///
    /// `mint[i]` declines the `i`th slot, leaving it rigid. A parameter whose
    /// bound already fixes its shape (`<F: fn(...)>`) is constrained
    /// structurally rather than by call-site inference, so minting a variable
    /// for it would only produce an unsolvable one.
    pub(super) fn instantiate(
        &mut self,
        slots: &[TypeId],
        mint: &[bool],
        of: &Instantiation<'_>,
    ) -> Instantiated {
        let mut vars = Vec::with_capacity(slots.len());
        let mut subst = IndexMap::default();
        for (i, &slot) in slots.iter().enumerate() {
            let named_slot = {
                let tt = self.tysys.type_table.borrow();
                match tt.get(slot) {
                    ResolvedType::TypeParam { index, name }
                    | ResolvedType::TypePack { index, name, .. } => Some((*index, name.clone())),
                    _ => None,
                }
            };
            let Some((index, name)) = named_slot.filter(|_| mint.get(i).copied().unwrap_or(true))
            else {
                vars.push(slot);
                continue;
            };
            let message = format!(
                "cannot infer type parameter `{name}` of {} `{}`; \
                 add a turbofish (`{}::<...>()`) or a type annotation",
                of.kind, of.name, of.name
            );
            let var = self.mint_infer_hole(of.span, message, name, Vec::new());
            subst.insert(index, var);
            vars.push(var);
        }
        Instantiated { vars, subst }
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
    /// A slot the solver left as its own variable stays unsolved, and
    /// `finalize_infer_holes` reports it.
    pub(super) fn record_instantiation(&mut self, inst: &Instantiated, solved: &[TypeId]) {
        for (&var, &answer) in inst.vars.iter().zip(solved.iter()) {
            if var != answer {
                self.solve_infer_var(var, answer);
            }
        }
    }
}
