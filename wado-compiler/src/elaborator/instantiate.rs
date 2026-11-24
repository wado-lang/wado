//! Instantiating a polymorphic signature at a use site. A declaration is written
//! in its own frame — `fn id<T>(x: T) -> T` names slot 0 a rigid `T` — but a use
//! of it stands for whatever this call needs, so each slot gets a fresh
//! [`ResolvedType::InferVar`] and the signature is rewritten into those before
//! anything is checked. They then join the lifecycle [`super::infer_hole`] owns.

use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::tir::{ResolvedType, TypeArgMap, TypeId, TypeParamId};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::call::slot_names;
use super::coercion::ExpectedReturn;
use super::infer_hole::BoundRows;
use super::scope::ParamRef;
use super::trait_query::SelfBinding;
use super::types::FunctionContext;
use crate::ast::{self, GenericParam};

/// The kind of declaration an [`Instantiation`] instantiates.
#[derive(Clone, Copy)]
pub(super) enum InstanceKind {
    Builtin,
    Function,
    Method,
    Struct,
    Variant,
}

impl InstanceKind {
    fn noun(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Variant => "variant",
        }
    }

    /// How a use site writes its turbofish after the name.
    fn turbofish_tail(self) -> &'static str {
        match self {
            Self::Struct => " { … }",
            Self::Variant => "::…",
            Self::Builtin | Self::Function | Self::Method => "()",
        }
    }

    /// What follows a slot's name where `name`'s use site leaves it unsettled.
    pub(super) fn slot_owner(self, name: &str) -> String {
        format!(
            "of {} `{name}`; add a turbofish (`{name}::<...>{}`) or a type annotation",
            self.noun(),
            self.turbofish_tail(),
        )
    }
}

/// What is being instantiated, for the "cannot infer" diagnostic raised if a
/// slot is never solved.
pub(super) struct Instantiation<'a> {
    pub(super) kind: InstanceKind,
    /// The declaration's name.
    pub(super) name: &'a str,
    /// Where the use site is.
    pub(super) span: Span,
    /// What the use site's turbofish names, in slot order: [`TypeTable::UNKNOWN`]
    /// where it wrote `_` or stopped short, empty where it wrote none.
    pub(super) type_args: &'a [TypeId],
    /// The receiver a bound's `Self::Assoc` projects off, where the site has
    /// one. A free function has none, and `Self` names nothing there.
    pub(super) self_binding: Option<SelfBinding>,
    /// What the receiver settles the enclosing impl's parameters to, which a
    /// method's bound may name. Empty off a method.
    pub(super) impl_args: &'a [(ParamRef, TypeId)],
}

/// A declaration's slots rewritten into one use site's variables.
pub(super) struct Instantiated {
    /// What each slot became, in declaration order: a fresh variable, or the
    /// slot itself where instantiation was declined.
    pub(super) vars: Vec<TypeId>,
    /// Rewrites a type written in the declaration's frame into this use site's
    /// variables. Apply with `TypeTable::substitute_type_params`.
    subst: TypeArgMap,
    /// Per slot, whether the use site leaves it to inference: not one left
    /// rigid, nor one the turbofish named.
    inferred: Vec<bool>,
    /// The associated-type equalities the slots' bounds state, linked once the
    /// arguments other than literals have met their slots.
    pub(super) links: Option<SlotLinks>,
}

/// What [`Elaborator::link_deferred_assoc_equalities`] reads of a call's slots.
#[derive(Clone)]
pub(super) struct SlotLinks {
    pub(super) owners: Vec<GenericParam>,
    pub(super) names: Vec<ParamRef>,
    pub(super) self_binding: Option<SelfBinding>,
    pub(super) span: Span,
}

impl Instantiated {
    /// Variables a use site minted for slots no declaration frame numbers,
    /// such as the `_`s a receiver's turbofish writes. Nothing substitutes
    /// into them, and an unsolved one reports nothing here: the site does.
    pub(super) fn of_vars(vars: Vec<TypeId>) -> Instantiated {
        Instantiated {
            inferred: vec![false; vars.len()],
            vars,
            subst: TypeArgMap::default(),
            links: None,
        }
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Instantiate `slots` — a declaration's type parameters, in order — with
    /// one fresh inference variable each. A type *pack* (`..T`) becomes one
    /// too, answered by a tuple: inside a tuple it is written as a
    /// [`ResolvedType::PackVar`], which the answer splices into.
    ///
    /// A slot [`Instantiation::type_args`] names is solved as it is minted.
    pub(super) fn instantiate(&mut self, slots: &[TypeId], of: &Instantiation<'_>) -> Instantiated {
        let mut vars = Vec::with_capacity(slots.len());
        let mut inferred = Vec::with_capacity(slots.len());
        let mut subst = TypeArgMap::default();
        let named_args = self.turbofish_per_slot(slots, of.type_args);
        for (i, &slot) in slots.iter().enumerate() {
            let named_slot = {
                let tt = self.tysys.type_table.borrow();
                let named = |id: &TypeParamId, is_pack| {
                    Some((*id, tt.param_name(*id).to_string(), is_pack))
                };
                match tt.get(slot) {
                    ResolvedType::TypeParam { id } => named(id, false),
                    ResolvedType::TypePack { id, .. } => named(id, true),
                    _ => None,
                }
            };
            let named = named_args
                .get(i)
                .copied()
                .filter(|&t| t != TypeTable::UNKNOWN);
            let Some((param, name, is_pack)) = named_slot else {
                vars.push(slot);
                inferred.push(false);
                continue;
            };
            // A pack the turbofish names is substituted outright: what it
            // stands for is written, so nothing is left to splice.
            if is_pack && let Some(pack) = named {
                subst.insert(param, pack);
                vars.push(pack);
                inferred.push(false);
                continue;
            }
            let var = self.mint_infer_var_named(&name);
            subst.insert(param, var);
            vars.push(var);
            // The parameter and the rest of the sentence stay apart so
            // `finalize_infer_holes` can name every unsolved slot of this use
            // site in one message.
            let owner = of.kind.slot_owner(of.name);
            // Answered here, so an argument meets what the source wrote. A `_`
            // in it reports as this slot would.
            if let Some(named) = named {
                self.blame_holes_as_slot(named, of.span, &name, &owner);
                self.settle_as_written(var, named);
                // A `_` standing for the whole slot leaves it to inference, as
                // an omitted argument does.
                inferred.push(self.tysys.type_table.borrow().is_infer_var(named));
                continue;
            }
            self.attach_infer_var_blame(var, of.span, name, owner);
            inferred.push(true);
        }
        Instantiated {
            vars,
            subst,
            inferred,
            links: None,
        }
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

    /// `type_id` with `owner`'s parameter `i` replaced by `type_args[i]`, read
    /// at this frame: a projection the substitution reaches is answered.
    pub(super) fn substitute_in_frame(
        &mut self,
        type_id: TypeId,
        owner: DefId,
        type_args: &[TypeId],
    ) -> TypeId {
        let substituted = self.tysys.substitute_type_params(type_id, owner, type_args);
        self.answer_frame_projections(substituted)
    }

    /// A static method's parameter types at a call: the parameters of a
    /// declaration in `impl_owners` filled from `impl_args`, and the method's
    /// own, which `method` declares, from `method_args`.
    pub(super) fn static_param_types_at(
        &mut self,
        param_types: Vec<TypeId>,
        impl_owners: &[DefId],
        method: Option<DefId>,
        impl_args: &[TypeId],
        method_args: &[TypeId],
    ) -> Vec<TypeId> {
        if impl_args.is_empty() && method_args.is_empty() {
            return param_types;
        }
        let subst: TypeArgMap = {
            let table = self.tysys.type_table.borrow();
            param_types
                .iter()
                .flat_map(|&t| table.type_params_in(t))
                .filter_map(|param| {
                    let args = if Some(param.owner) == method {
                        method_args
                    } else if impl_owners.contains(&param.owner) {
                        impl_args
                    } else {
                        return None;
                    };
                    Some((param, *args.get(param.index as usize)?))
                })
                .collect()
        };
        param_types
            .iter()
            .map(|&t| self.substitute_map_in_frame(&subst, t))
            .collect()
    }

    /// `subst` over `type_id`, then the projections that answers.
    pub(super) fn substitute_map_in_frame(
        &mut self,
        subst: &TypeArgMap,
        type_id: TypeId,
    ) -> TypeId {
        let substituted = self
            .tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(type_id, subst);
        self.answer_frame_projections(substituted)
    }

    /// `inst`'s variables as the context answers them, or `None` while any is
    /// still open.
    pub(super) fn answered_slots(&self, inst: &Instantiated) -> Option<Vec<TypeId>> {
        let answers = self.apply_infer_holes_each(&inst.vars);
        let all = answers
            .iter()
            .zip(&inst.vars)
            .all(|(answer, var)| answer != var);
        all.then_some(answers)
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
    /// body's sweep substitutes the variables out of every recorded fact.
    /// A slot still open is reported by the blame it was minted with.
    pub(super) fn record_instantiation(&mut self, inst: &Instantiated, solved: &[TypeId]) {
        for (&var, &answer) in inst.vars.iter().zip(solved.iter()) {
            // A slot instantiation declined stays rigid.
            if !self.tysys.type_table.borrow().is_infer_var(var) {
                continue;
            }
            self.equate(var, answer);
            // An inferred `!` binds nothing, yet stands for the slot: the
            // site settled it.
            if self.apply_infer_holes(answer) == TypeTable::NEVER {
                self.waive_open(var);
            }
        }
    }

    /// Leave unreported each of `inst`'s variables nothing answered: the site
    /// gave the instance up, and reports what it could not settle itself.
    pub(super) fn abandon(&mut self, inst: &Instantiated) {
        for &var in &inst.vars {
            self.waive_open(var);
        }
    }

    /// Instantiate `slots`, carry their bounds, resolve the arguments against
    /// them, and settle the answers back. One call, so no path can take part of
    /// the sequence and hand a closure a rigid slot nothing can construct.
    /// `ret` declares its return type in the slots' frame. The instantiation
    /// comes back for the call's inference to solve: a second one would leave
    /// the variables the arguments were typed by unanswered.
    pub(super) fn resolve_args_through_slots(
        &mut self,
        ctx: &mut FunctionContext,
        args_ast: &[ast::Expr],
        param_types: &[TypeId],
        slots: &[TypeId],
        own_params: &[GenericParam],
        of: &Instantiation<'_>,
        ret: Option<ExpectedReturn>,
    ) -> (Vec<TypeId>, Option<Instantiated>) {
        if slots.is_empty() {
            let args = self.resolve_args_against_params(args_ast, ctx, param_types, None, None);
            return (args, None);
        }
        let mut inst = self.instantiate(slots, of);
        self.record_slot_bounds(&inst, own_params, of.self_binding, of.impl_args, of.span);
        inst.links = Some(SlotLinks {
            owners: own_params.to_vec(),
            names: slot_names(own_params, &self.tysys.resolutions),
            self_binding: of.self_binding,
            span: of.span,
        });
        let param_types = self.instantiate_types(param_types, &inst);
        let ret = ret.map(|ret| ExpectedReturn {
            declared: self.instantiate_type(ret.declared, &inst),
            ..ret
        });
        let mut args =
            self.resolve_args_against_params(args_ast, ctx, &param_types, Some(&inst), ret);
        self.settle_onto_slots(&mut args, ret);
        (args, Some(inst))
    }

    /// Settle the walk's variables, substituting through `args`. A slot no
    /// argument answered is answered by `ret`, the type the call's site
    /// expects. One still open stays a variable of this site: the
    /// declaration's own parameter means nothing here (#2574).
    pub(super) fn settle_onto_slots(&mut self, args: &mut [TypeId], ret: Option<ExpectedReturn>) {
        if let Some(ret) = ret {
            self.equate(ret.declared, ret.expected);
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
        self_binding: Option<SelfBinding>,
        impl_args: &[(ParamRef, TypeId)],
        span: Span,
    ) {
        for ((&var, param), &inferred) in inst.vars.iter().zip(params).zip(&inst.inferred) {
            if !inferred {
                continue;
            }
            let bounds = self.declared_bounds(param, self_binding, impl_args, BoundRows::Unbound);
            self.attach_infer_var_bounds(var, param.name.clone(), bounds, span);
        }
    }
}
