//! A method call or comparison whose impl depends on what the receiver's
//! variables settle to. The call is typed by the trait's declaration of the
//! method, with `Self` the receiver, and its selection waits as an
//! [`Obligation::Pending`]. Once the body ends the receiver is settled as far as
//! it will be: the selection is made there, the impl's signature is held to what
//! the call was typed with, and the recorded dispatch is pointed at the impl.

use crate::compiler_host::CompilerHost;
use crate::defs::DefId;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::name::{FqTraitName, FqTypeName, RefKind};
use crate::tir::{
    EffectRows, FunctionRef, SlotProjections, TypeArgMap, TypeId, TypeParamId, slots_from_impl_args,
};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::expr::IndexAccess;
use super::infer_hole::Obligation;
use super::method_call::InheritedFrame;
use super::static_call::ReceiverId;
use super::synth::ArgSource;
use super::trait_env::ImplTargetKey;
use super::types::{
    MethodInfo, MethodOwner, RequiredTrait, TraitLookup, TraitMethodMatch, TypeError,
};
use crate::ast::{AstId, BinaryOp, UnaryOp};
use crate::compiler_item::CompilerItem;

/// What [`Elaborator::dispatch_function_ref`] names a call's function after.
pub(super) struct DispatchSite<'a> {
    pub(super) owner: MethodOwner,
    pub(super) base_type_id: TypeId,
    pub(super) method_name: &'a str,
    pub(super) trait_name: Option<FqTraitName>,
    pub(super) matched_impl_decl: Option<DefId>,
    /// Set where a `&T` / `&mut T` impl answered, which names the call after
    /// the reference.
    pub(super) matched_ref_kind: Option<RefKind>,
    pub(super) trait_impl_struct_name: Option<FqTypeName>,
    pub(super) trait_impl_module_source: Option<ModuleSource>,
    pub(super) inherent_impl_module: Option<ModuleSource>,
    /// Where the receiver is declared, which is where its inherent methods live.
    pub(super) struct_module: ModuleSource,
    pub(super) blanket_binder: Option<FqTypeName>,
    pub(super) dispatched_method_def: Option<DefId>,
    pub(super) dispatched_impl_block: Option<DefId>,
    pub(super) from_concrete_impl: bool,
    pub(super) is_ref_impl: bool,
    pub(super) cm_name: Option<String>,
    pub(super) impl_slot_args: Vec<TypeId>,
    pub(super) trait_args: Vec<TypeId>,
    pub(super) method_type_args: Vec<TypeId>,
}

/// What a trait match found on a receiver's value gives its call to dispatch
/// with.
pub(super) struct AdoptedMatch {
    pub(super) info: MethodInfo,
    pub(super) trait_name: FqTraitName,
    pub(super) matched_impl_decl: Option<DefId>,
    pub(super) trait_impl_struct_name: Option<FqTypeName>,
    pub(super) trait_impl_module_source: Option<ModuleSource>,
    pub(super) blanket_binder: Option<FqTypeName>,
}

/// A call waiting for its receiver to settle before it selects.
pub(super) enum PendingSelection {
    /// A method call, whose selection is made into the numbered slot the
    /// sweep writes over its recorded dispatch.
    Call(usize, PendingCall),
    /// An operator, which records its dispatch once it selects.
    Operator(PendingOperator),
    /// A static call, which records its dispatch once it selects.
    Static(Box<PendingStatic>),
}

/// What becomes of a selection nothing has decided where its walk ends.
#[derive(Clone, Copy)]
pub(super) enum Undecided {
    /// The walk was its body: nothing later settles what it waits on.
    Report,
    /// The walk was part of a body, whose later lines may still settle it.
    Wait,
}

/// A static call through a trait, typed by the trait's declaration.
pub(super) struct PendingStatic {
    pub(super) receiver: ReceiverId,
    /// The receiver as written, which the call resolves on.
    pub(super) receiver_type: TypeId,
    /// The instance the impl is selected for.
    pub(super) instance: TypeId,
    pub(super) method_name: String,
    pub(super) args: Vec<TypeId>,
    pub(super) impl_args: Vec<TypeId>,
    pub(super) call: AstId,
    /// The method name's token, whose use→def edge names the selected method.
    pub(super) method_id: Option<AstId>,
    pub(super) span: Span,
    pub(super) expected: Option<TypeId>,
    pub(super) return_type: TypeId,
}

pub(super) enum PendingOp {
    Binary {
        op: BinaryOp,
        right: TypeId,
        right_span: Span,
    },
    Unary(UnaryOp),
    Index {
        /// The subscripted value, references and all.
        receiver: TypeId,
        access: IndexAccess,
        key: TypeId,
    },
    IndexAssign {
        key: TypeId,
    },
}

impl PendingOp {
    /// The key an indexing trait is asked at.
    fn key(&self) -> Option<TypeId> {
        match self {
            Self::Index { key, .. } | Self::IndexAssign { key } => Some(*key),
            Self::Binary { .. } | Self::Unary(_) => None,
        }
    }
}

pub(super) struct PendingOperator {
    pub(super) op: PendingOp,
    /// The operator trait the impl is selected for.
    pub(super) item: CompilerItem,
    /// The operand the impl is selected on.
    pub(super) left: TypeId,
    pub(super) span: Span,
    pub(super) origin: AstId,
    /// What the operator was typed with: `bool`, or a variable for `Output`.
    pub(super) result: TypeId,
}

pub(super) struct PendingCall {
    pub(super) call: AstId,
    /// The method name's token, whose use→def edge names the selected method.
    pub(super) method_id: Option<AstId>,
    /// The receiver's value, as the lookup reads it.
    pub(super) receiver: TypeId,
    /// Set where the impls waited on were written for `&T` / `&mut T`, which
    /// the selection is then made among.
    pub(super) ref_kind: Option<RefKind>,
    pub(super) method_name: String,
    pub(super) args: Vec<TypeId>,
    /// What the call was typed with, from the trait's declaration.
    pub(super) param_types: Vec<TypeId>,
    pub(super) return_type: TypeId,
    pub(super) span: Span,
}

/// The dispatch a pending selection made, which the sweep writes over the
/// call's recorded one.
#[derive(Clone)]
pub(crate) struct SelectedDispatch {
    pub(crate) method_def: Option<DefId>,
    pub(crate) function_ref: FunctionRef,
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// What `trait_match` dispatches a call on `base_type_id` with.
    pub(super) fn adopt_trait_match(
        &self,
        trait_match: TraitMethodMatch,
        base_type_id: TypeId,
        required_trait: Option<&RequiredTrait>,
    ) -> AdoptedMatch {
        let mut info = trait_match.method_info;
        // A blanket answers a parameter's call only until the instance names
        // its type, where an impl written for that type outranks it. So a call
        // naming the trait's arguments goes as one through a bound does: on the
        // receiver, to the trait at those arguments, dispatched there. It keeps
        // the blanket's block, which the body is checked against and the
        // instance's impl must agree with.
        if trait_match.blanket_binder.is_some()
            && self
                .tysys
                .type_table
                .borrow()
                .receiver_head_awaits_substitution(base_type_id)
            && let Some(asked) = required_trait.and_then(|r| r.args.clone())
        {
            let names: Vec<FqTypeName> = {
                let tt = self.tysys.type_table.borrow();
                asked.iter().map(|&arg| tt.fq_type_name(arg)).collect()
            };
            info.impl_slot_args = Vec::new();
            info.trait_args = asked;
            return AdoptedMatch {
                info,
                trait_name: trait_match.trait_name.with_args(names),
                matched_impl_decl: None,
                trait_impl_struct_name: None,
                trait_impl_module_source: None,
                blanket_binder: None,
            };
        }
        let trait_name = trait_match.trait_name;
        let mut trait_impl_struct_name = None;
        // A newtype inherits a trait impl its base wrote, and the impl spells
        // `Self` as that base. Name the link the impl sits on, so the signature
        // re-types to the receiver the way an inherited inherent method's does,
        // and the call is named as a call on the link: a block written at one
        // instantiation keeps its arguments (issue #2644).
        if let Some(trait_def) = trait_name.canonical()
            && let Some(link) = match info.impl_block {
                Some(block) => self.tysys.block_link(base_type_id, trait_def, block),
                None => self.tysys.impl_link(base_type_id, trait_def),
            }
            && link != base_type_id
            && info.owner == MethodOwner::Receiver
        {
            info.owner = if self.tysys.link_behind_reference(base_type_id, link) {
                MethodOwner::Referent(link)
            } else {
                MethodOwner::InheritedFrom(link)
            };
        } else if trait_match.impl_struct_fq.head()
            != self.tysys.fq_receiver_head(base_type_id).head()
        {
            trait_impl_struct_name = Some(trait_match.impl_struct_fq.clone());
        }
        AdoptedMatch {
            info,
            trait_name,
            matched_impl_decl: trait_match.impl_struct_fq.head().def(),
            trait_impl_struct_name,
            trait_impl_module_source: Some(trait_match.impl_module_source),
            blanket_binder: trait_match.blanket_binder,
        }
    }

    /// The trait's own declaration of `method_name`, with `Self` the receiver,
    /// where the impls `live` names are all of one trait and the declaration
    /// types the call without them. Each trait argument is a variable the
    /// call's arguments may settle, and the impl selected settles otherwise.
    ///
    /// `None` where the call cannot wait: an impl written for no block, a
    /// receiver whose head is open or that names a parameter, or a method whose
    /// signature reads an associated type or a type parameter of its own.
    pub(super) fn trait_declared_info(
        &mut self,
        live: &[Option<DefId>],
        receiver: TypeId,
        method_name: &str,
    ) -> Option<(FqTraitName, MethodInfo)> {
        {
            let tt = self.tysys.type_table.borrow();
            if !tt.head_is_known(receiver) || tt.contains_rigid_param(receiver) {
                return None;
            }
        }
        let traits: IndexSet<DefId> = live
            .iter()
            .map(|def| def.and_then(|def| self.tysys.trait_env.impl_headers[&def].trait_def()))
            .collect::<Option<_>>()?;
        let &[decl] = traits.iter().collect::<Vec<_>>().as_slice() else {
            return None;
        };
        let (sig, _) = self.trait_method_of(decl, method_name)?;
        let reads_assoc_type = {
            let tt = self.tysys.type_table.borrow();
            sig.decl
                .param_types
                .iter()
                .chain(&sig.decl.return_type)
                .any(|&ty| tt.contains_assoc_type_projection(ty))
        };
        if reads_assoc_type || !sig.own_type_params().is_empty() {
            return None;
        }
        let arity = self.tysys.trait_env.decl_header_of(decl)?.type_params.len();
        let mut slots = TypeArgMap::default();
        slots.insert(TypeParamId::trait_self(*decl), receiver);
        let mut trait_args = Vec::with_capacity(arity);
        for index in 1..=arity {
            let var = self.mint_infer_var();
            let index = u32::try_from(index).expect("a trait's parameters fit a slot index");
            slots.insert(
                TypeParamId {
                    owner: *decl,
                    index,
                },
                var,
            );
            trait_args.push(var);
        }
        let instantiated = sig.instantiate_received(
            &self.tysys.type_table,
            &slots,
            &SlotProjections::default(),
            &EffectRows::default(),
        );
        let trait_name = FqTraitName::declared(self.tysys.resolutions.defs(), *decl);
        Some((
            trait_name,
            MethodInfo {
                trait_args,
                ..MethodInfo::of_sig(&sig, instantiated)
            },
        ))
    }

    /// Queue `pending`, its recorded dispatch to be overwritten by the one its
    /// selection makes.
    pub(super) fn wait_for_selection(&mut self, pending: PendingCall) {
        let id = self.infer_holes.reserve_selection();
        self.sem
            .types
            .method_dispatch
            .get_mut(&pending.call)
            .expect("a waiting call recorded its dispatch")
            .selection = Some(id);
        self.infer_holes
            .require(Obligation::Pending(PendingSelection::Call(id, pending)));
    }

    /// Queue `pending`, which records its dispatch once it selects.
    pub(super) fn wait_for_operator(&mut self, pending: PendingOperator) {
        self.infer_holes
            .require(Obligation::Pending(PendingSelection::Operator(pending)));
    }

    /// The trait's declaration of the static `method_name` on `receiver`,
    /// called with `arity` arguments, where which impl answers waits on a
    /// variable the receiver holds and the call can wait for it: as
    /// [`Self::trait_declared_info`] says, and omitting no default.
    pub(super) fn static_waits(
        &mut self,
        receiver: TypeId,
        method_name: &str,
        arity: usize,
    ) -> Option<MethodInfo> {
        let receiver = self.apply_infer_holes(receiver);
        let live = self.undecided_impls(receiver, method_name, None)?;
        let (_, info) = self.trait_declared_info(&live, receiver, method_name)?;
        (info.param_types.len() == arity).then_some(info)
    }

    /// Queue `pending`, which records its dispatch once it selects.
    pub(super) fn wait_for_static(&mut self, pending: PendingStatic) {
        self.infer_holes
            .require(Obligation::Pending(PendingSelection::Static(Box::new(
                pending,
            ))));
    }

    /// Resolve the static call `pending` waits for, or hand it back while its
    /// receiver still leaves the impl undecided.
    fn select_static(&mut self, pending: Box<PendingStatic>) -> Option<Box<PendingStatic>> {
        let instance = self.apply_infer_holes(pending.instance);
        if self.selection_undecided(instance, &pending.method_name, None) {
            return Some(pending);
        }
        let receiver_type = self.apply_infer_holes(pending.receiver_type);
        let args: Vec<TypeId> = pending
            .args
            .iter()
            .map(|&a| self.apply_infer_holes(a))
            .collect();
        let impl_args: Vec<TypeId> = pending
            .impl_args
            .iter()
            .map(|&a| self.apply_infer_holes(a))
            .collect();
        let owner = self.static_owner_key(pending.receiver.key(), &pending.method_name);
        let inherited = self.inherited_static(pending.receiver.key(), &owner, &impl_args);
        let answered = self.resolve_static_method_call_from_qualified(
            &pending.receiver,
            receiver_type,
            &pending.method_name,
            &args,
            &impl_args,
            &[],
            inherited,
            pending.call,
            pending.span,
            pending.expected,
            None,
        );
        if answered != TypeTable::ERROR {
            self.hold_to_selected(answered, pending.return_type, pending.span);
        }
        if let Some(method_id) = pending.method_id
            && let Some(def) = self
                .sem
                .types
                .static_method_dispatch
                .get(&pending.call)
                .and_then(|dispatch| dispatch.method_def)
        {
            self.record_reference_to_decl(method_id, def, pending.span);
        }
        None
    }

    /// Run `walk`, then make the selections it left waiting while its scope
    /// and the facts it records into are still the ones the sites were walked
    /// in. Those waiting from before the walk wait on.
    pub(super) fn settling_selections<R>(
        &mut self,
        undecided: Undecided,
        walk: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let outer = self.infer_holes.take_pending();
        let result = walk(self);
        self.settle_pending_selections(undecided);
        for selection in outer {
            self.infer_holes.require(Obligation::Pending(selection));
        }
        result
    }

    /// Make each pending selection whose receiver has settled far enough,
    /// until none can be made, and treat the rest as `undecided` says.
    /// A selection settles variables in turn, through the impl's signature, so
    /// a later one may decide only after an earlier one.
    pub(super) fn settle_pending_selections(&mut self, undecided: Undecided) {
        loop {
            let pending = self.infer_holes.take_pending();
            if pending.is_empty() {
                return;
            }
            let asked = pending.len();
            let waiting: Vec<PendingSelection> = pending
                .into_iter()
                .filter_map(|selection| match selection {
                    PendingSelection::Call(id, call) => self
                        .select_call(id, call)
                        .map(|call| PendingSelection::Call(id, call)),
                    PendingSelection::Operator(op) => {
                        self.select_operator(op).map(PendingSelection::Operator)
                    }
                    PendingSelection::Static(call) => {
                        self.select_static(call).map(PendingSelection::Static)
                    }
                })
                .collect();
            if waiting.len() == asked {
                for selection in waiting {
                    match undecided {
                        Undecided::Report => self.report_stuck_selection(selection),
                        Undecided::Wait => {
                            self.infer_holes.require(Obligation::Pending(selection));
                        }
                    }
                }
                return;
            }
            for selection in waiting {
                self.infer_holes.require(Obligation::Pending(selection));
            }
            self.run_obligations();
        }
    }

    /// Report `selection` as needing an annotation, unless a report on another
    /// selection already closed the variables it waits on.
    fn report_stuck_selection(&mut self, selection: PendingSelection) {
        let (receiver, method, span) = match &selection {
            PendingSelection::Call(_, call) => (call.receiver, call.method_name.clone(), call.span),
            PendingSelection::Operator(op) => {
                (op.left, self.tysys.operator_method_name(op.item), op.span)
            }
            PendingSelection::Static(call) => (call.instance, call.method_name.clone(), call.span),
        };
        if self.type_has_infer_hole(self.apply_infer_holes(receiver)) {
            self.report_undecided_selection(receiver, &method, span);
        }
        if let PendingSelection::Operator(op) = selection
            && self.tysys.type_table.borrow().is_infer_var(op.result)
        {
            self.close_infer_var(op.result);
        }
    }

    /// Make the selection `call` waits for, or hand it back while its receiver
    /// still leaves it undecided.
    fn select_call(&mut self, id: usize, call: PendingCall) -> Option<PendingCall> {
        let receiver = self.apply_infer_holes(call.receiver);
        let target = match call.ref_kind {
            Some(kind) => ImplTargetKey::Ref(kind),
            None => self.impl_target_of(receiver),
        };
        let lookup = self.find_trait_method_for_type(
            &target,
            &call.method_name,
            self.tysys.impl_position_args(receiver).as_deref(),
            Some(receiver),
            call.span,
            None,
            Some(ArgSource::Types(call.args.clone())),
        );
        match lookup {
            TraitLookup::Undecided(_) => return Some(call),
            TraitLookup::Found(found) => self.select_pending(id, &call, receiver, *found),
            TraitLookup::Missing => {
                if !self.report_unmet_impl_bound(receiver, call.span, |s, def| {
                    s.impl_offers_method(def, &call.method_name, None)
                }) {
                    let type_name = self.tysys.type_table.borrow().name(receiver);
                    let _ = self.emit(TypeError::MethodNotFound {
                        type_name,
                        method_name: call.method_name.clone(),
                        hint: String::new(),
                        span: call.span,
                    });
                }
            }
            TraitLookup::Reported => {}
        }
        None
    }

    /// Resolve the operator `pending` waits for as written, or hand it back
    /// while its left operand still leaves the impl undecided.
    fn select_operator(&mut self, pending: PendingOperator) -> Option<PendingOperator> {
        let left = self.apply_infer_holes(pending.left);
        let key = pending.op.key().map(|key| self.apply_infer_holes(key));
        if self.selection_waits(left, pending.item, key) {
            return Some(pending);
        }
        let answered = match pending.op {
            PendingOp::Binary {
                op,
                right,
                right_span,
            } => {
                let right = self.apply_infer_holes(right);
                self.resolve_binary_op(
                    left,
                    op,
                    right,
                    right_span,
                    pending.span,
                    Some(pending.origin),
                )
            }
            PendingOp::Unary(op) => self
                .dispatch_unary_operator(op, left, pending.span, pending.origin)
                .expect("an operand whose impl waited dispatches through a trait"),
            PendingOp::Index {
                receiver,
                access,
                key,
            } => {
                let receiver = self.apply_infer_holes(receiver);
                let key = self.apply_infer_holes(key);
                self.dispatch_index(pending.origin, pending.span, receiver, access, key)
            }
            PendingOp::IndexAssign { .. } => {
                let key = key.expect("a subscript selects at its key");
                self.dispatch_index_assign(pending.origin, pending.span, left, key)
            }
        };
        if answered != TypeTable::ERROR {
            self.hold_to_selected(answered, pending.result, pending.span);
        }
        None
    }

    /// Point pending selection `id` at the impl `found` answers it with, and
    /// hold the impl's signature to what the call was typed with.
    fn select_pending(
        &mut self,
        id: usize,
        selection: &PendingCall,
        receiver: TypeId,
        found: TraitMethodMatch,
    ) {
        let ref_owner = selection
            .ref_kind
            .map(|_| found.owner_for(receiver, &self.tysys.type_table.borrow()));
        let impl_struct_fq = found.impl_struct_fq.clone();
        let mut adopted = self.adopt_trait_match(found, receiver, None);
        if let Some(owner) = ref_owner {
            adopted.info.is_ref_impl = true;
            adopted.info.owner = owner;
            adopted.trait_impl_struct_name = Some(impl_struct_fq);
        }
        let info = adopted.info;
        let mut param_types = info.param_types.clone();
        let mut return_type = info.return_type;
        if let Some(base) = info.owner.newtype_base() {
            let frame = InheritedFrame {
                base,
                newtype: self.tysys.get_base_type(receiver),
            };
            frame.to_newtype_in(&self.tysys, &mut param_types);
            return_type = frame.to_newtype(&self.tysys, return_type);
        }
        if let Some(block) = info.impl_block {
            self.settle_trait_arg_slots(
                block,
                &slots_from_impl_args(block, &info.impl_slot_args),
                &info.inferred_impl_slots,
                &mut param_types,
                &selection.args,
                &mut return_type,
                selection.span,
            );
        }
        for (&typed, &declared) in selection.param_types.iter().zip(&param_types) {
            self.hold_to_selected(typed, declared, selection.span);
        }
        self.hold_to_selected(return_type, selection.return_type, selection.span);
        if let (Some(method_id), Some(def)) = (selection.method_id, info.method_def) {
            self.record_reference_to_decl(method_id, def, selection.span);
        }
        let struct_module = self
            .tysys
            .type_table
            .borrow()
            .impl_home(receiver)
            .unwrap_or_else(|| self.current_module_source.clone());
        let function_ref = self.dispatch_function_ref(DispatchSite {
            owner: info.owner,
            base_type_id: receiver,
            method_name: &selection.method_name,
            trait_name: Some(adopted.trait_name),
            matched_impl_decl: adopted.matched_impl_decl,
            matched_ref_kind: selection.ref_kind,
            trait_impl_struct_name: adopted.trait_impl_struct_name,
            trait_impl_module_source: adopted.trait_impl_module_source,
            inherent_impl_module: info.impl_module,
            struct_module,
            blanket_binder: adopted.blanket_binder,
            dispatched_method_def: info.method_def,
            dispatched_impl_block: info.impl_block,
            from_concrete_impl: info.from_concrete_impl,
            is_ref_impl: info.is_ref_impl,
            cm_name: info.cm_name,
            impl_slot_args: info.impl_slot_args,
            trait_args: info.trait_args,
            method_type_args: Vec::new(),
        });
        self.infer_holes.select(
            id,
            SelectedDispatch {
                method_def: info.method_def,
                function_ref,
            },
        );
    }

    /// Equate what the call was typed with and what the selected impl
    /// declares, and report where they still differ.
    fn hold_to_selected(&mut self, actual: TypeId, expected: TypeId, span: Span) {
        self.equate(actual, expected);
        let actual = self.apply_infer_holes(actual);
        let expected = self.apply_infer_holes(expected);
        if let Err(payload) = self.tysys.typecheck(actual, expected) {
            let _ = self.emit(TypeError::TypeMismatch {
                expected: payload.expected,
                found: payload.found,
                span,
            });
        }
    }
}
