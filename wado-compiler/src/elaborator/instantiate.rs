//! Instantiating a polymorphic signature at a use site. A declaration is written
//! in its own frame — `fn id<T>(x: T) -> T` names slot 0 a rigid `T` — but a use
//! of it stands for whatever this call needs, so each slot gets a fresh
//! [`ResolvedType::InferVar`] and the signature is rewritten into those before
//! anything is checked. They then join the lifecycle [`super::infer_hole`] owns.

use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::tir::{ResolvedType, SubstitutionContext, TypeArgMap, TypeId};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::call::bound_targets;
use super::coercion::ExpectedReturn;
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
            Self::Builtin | Self::Function | Self::Method | Self::Variant => "()",
        }
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
    /// slot itself where instantiation was declined. Feed this to
    /// `InferCtx::new` so `solve` answers positionally.
    pub(super) vars: Vec<TypeId>,
    /// Rewrites a type written in the declaration's frame into this use site's
    /// variables. Apply with `TypeTable::substitute_type_params`.
    subst: TypeArgMap,
    /// Per-slot "cannot infer" diagnostic, attached by
    /// [`Elaborator::record_instantiation`] to the slots still unsolved when the
    /// use site commits. `None` marks a slot that cannot go unanswered: one left
    /// rigid, or one the turbofish named.
    diags: Vec<Option<(Span, String, String)>>,
}

impl Instantiated {
    /// Variables a use site minted for slots no declaration frame numbers,
    /// such as the `_`s a receiver's turbofish writes. Nothing substitutes
    /// into them, and an unsolved one reports nothing here: the site does.
    pub(super) fn of_vars(vars: Vec<TypeId>) -> Instantiated {
        let diags = vec![None; vars.len()];
        Instantiated {
            vars,
            subst: TypeArgMap::default(),
            diags,
        }
    }
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
    /// A slot [`Instantiation::type_args`] names is solved as it is minted. A
    /// pack the turbofish names is substituted outright: what it stands for is
    /// written, so a parameter type mentioning it is concrete before any
    /// argument meets it, and nothing is left for the unifier to splice.
    pub(super) fn instantiate(&mut self, slots: &[TypeId], of: &Instantiation<'_>) -> Instantiated {
        let mut vars = Vec::with_capacity(slots.len());
        let mut diags = Vec::with_capacity(slots.len());
        let mut subst = TypeArgMap::default();
        let named_args = self.turbofish_per_slot(slots, of.type_args);
        for (i, &slot) in slots.iter().enumerate() {
            let named_slot = {
                let tt = self.tysys.type_table.borrow();
                match tt.get(slot) {
                    ResolvedType::TypeParam { id, name } => Some((*id, name.clone(), false)),
                    ResolvedType::TypePack { id, name, .. } => Some((*id, name.clone(), true)),
                    _ => None,
                }
            };
            let named = named_args
                .get(i)
                .copied()
                .filter(|&t| t != TypeTable::UNKNOWN);
            let Some((param, name, is_pack)) = named_slot else {
                vars.push(slot);
                diags.push(None);
                continue;
            };
            if is_pack {
                if let Some(pack) = named {
                    subst.insert(param, pack);
                }
                vars.push(slot);
                diags.push(None);
                continue;
            }
            let var = self.mint_infer_var_named(&name);
            subst.insert(param, var);
            vars.push(var);
            // Answered here, so an argument meets what the source wrote.
            if let Some(named) = named {
                self.solve_infer_var_as_written(var, named);
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
                    "of {} `{}`; add a turbofish (`{}::<...>{}`) or a type annotation",
                    of.kind.noun(),
                    of.name,
                    of.name,
                    of.kind.turbofish_tail(),
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

    /// A static method's parameter types at a call. The declarations in
    /// `owners`, its impl block's and its own, number one sequence: the impl's
    /// parameters first and the method's after them.
    pub(super) fn static_param_types_at(
        &mut self,
        param_types: Vec<TypeId>,
        owners: &[DefId],
        impl_args: &[TypeId],
        method_args: &[TypeId],
    ) -> Vec<TypeId> {
        if impl_args.is_empty() && method_args.is_empty() {
            return param_types;
        }
        let combined: Vec<TypeId> = impl_args.iter().chain(method_args).copied().collect();
        let subst: TypeArgMap = {
            let table = self.tysys.type_table.borrow();
            param_types
                .iter()
                .flat_map(|&t| table.type_params_in(t))
                .filter(|param| owners.contains(&param.owner))
                .filter_map(|param| Some((param, *combined.get(param.index as usize)?)))
                .collect()
        };
        param_types
            .iter()
            .map(|&t| {
                let substituted = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .substitute_type_params(t, &subst);
                self.answer_frame_projections(substituted)
            })
            .collect()
    }

    /// [`Self::substitute_in_frame`] for a substitution bound by name.
    pub(super) fn substitute_ctx_in_frame(
        &mut self,
        subst: &SubstitutionContext,
        type_id: TypeId,
    ) -> TypeId {
        let substituted = subst.substitute(type_id, &mut self.tysys.type_table.borrow_mut());
        self.answer_frame_projections(substituted)
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
        let inst = self.instantiate(slots, of);
        self.record_slot_bounds(&inst, own_params, of.self_binding, of.impl_args, of.span);
        let param_types = self.instantiate_types(param_types, &inst);
        let ret = ret.map(|ret| ExpectedReturn {
            declared: self.instantiate_type(ret.declared, &inst),
            ..ret
        });
        let mut args =
            self.resolve_args_against_params(args_ast, ctx, &param_types, Some(&inst), ret);
        self.settle_onto_slots(&inst, &mut args, ret);
        (args, Some(inst))
    }

    /// Settle the walk's variables, substituting through `args`. A slot no
    /// argument answered is answered by `ret`, the type the call's site
    /// expects. One still open stays a variable of this site: the
    /// declaration's own parameter means nothing here (#2574).
    pub(super) fn settle_onto_slots(
        &mut self,
        inst: &Instantiated,
        args: &mut [TypeId],
        ret: Option<ExpectedReturn>,
    ) {
        if let Some(ret) = ret {
            let declared = self.apply_infer_holes(ret.declared);
            let expected = self.apply_infer_holes(ret.expected);
            self.solve_own_infer_holes_against(declared, expected, &inst.vars);
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
        for ((&var, param), diag) in inst.vars.iter().zip(params.iter()).zip(inst.diags.iter()) {
            // `diags` is `None` exactly where instantiation was declined, and
            // that slot is rigid: it is never solved, so a bound recorded
            // against it could never be re-checked.
            if diag.is_none() {
                continue;
            }
            let bounds = self.declared_bounds(param, self_binding, impl_args);
            self.attach_infer_var_bounds(var, param.name.clone(), bounds, span);
        }
    }

    /// Stand a variable in for each pack slot that a slot `solved` left open
    /// pins through an associated-type equality
    /// (`S: ReflectStruct<FieldTypes = [..F]>`). [`Self::instantiate`] leaves a
    /// pack rigid, and only the owner's projection answers this one; it is
    /// blamed with its owner.
    pub(super) fn defer_pinned_packs(
        &mut self,
        inst: &Instantiated,
        params: &[GenericParam],
        names: &[ParamRef],
        solved: &mut [TypeId],
        self_binding: Option<SelfBinding>,
        impl_args: &[(ParamRef, TypeId)],
    ) {
        for (i, param) in params.iter().enumerate() {
            let Some((span, _, owner)) =
                inst.diags[i].clone().filter(|_| solved[i] == inst.vars[i])
            else {
                continue;
            };
            let targets = param
                .bounds
                .iter()
                .flat_map(|b| bound_targets(b, names, &self.tysys.resolutions));
            for (_, j) in targets.collect::<Vec<_>>() {
                let pack = &params[j];
                if !pack.is_pack || solved[j] != inst.vars[j] {
                    continue;
                }
                let var = self.mint_infer_var_named(&pack.name);
                self.attach_infer_var_blame(var, span, pack.name.clone(), owner.clone());
                let bounds = self.declared_bounds(pack, self_binding, impl_args);
                self.attach_infer_var_bounds(var, pack.name.clone(), bounds, span);
                solved[j] = var;
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
            if let Some((span, param, owner)) = inst.diags[i].clone() {
                self.attach_infer_var_blame(var, span, param, owner);
            }
        }
    }
}
