//! The instance a call reaches: the one answer collection queues and the call
//! rewrite looks up, keyed by the template the call recorded.

use crate::elaborator::trait_env::{BlanketImpl, BlanketParamSource, BlanketReceiver};
use crate::hashmap::IndexMap;
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TemplateId, TirExpr,
    TirExprKind, TirFunction, TypeId, TypeTable,
};

use super::state::Monomorphizer;
use super::{Templates, generic_function_name};
use crate::synthesis::template::blanket_impl_args;

/// What a call site says about the instance it reaches.
struct CallSite<'a> {
    template: &'a TemplateId,
    monomorph: Option<&'a MonomorphInfo>,
    type_args: &'a [TypeId],
    receiver: Option<TypeId>,
    args: &'a [CallArg],
}

impl<'a> CallSite<'a> {
    fn of(expr: &'a TirExpr) -> Option<Self> {
        match &expr.kind {
            TirExprKind::Call {
                func,
                type_args,
                args,
                has_receiver,
            } => {
                let FunctionRef {
                    template: Some(template),
                    monomorph_info,
                    ..
                } = &**func
                else {
                    return None;
                };
                let (receiver, args) = if *has_receiver {
                    let (receiver, rest) = args.split_first()?;
                    (Some(receiver.expr.type_id), rest)
                } else {
                    (None, args.as_slice())
                };
                Some(Self {
                    template,
                    monomorph: monomorph_info.as_ref(),
                    type_args,
                    receiver,
                    args,
                })
            }
            TirExprKind::FuncRef {
                template: Some(template),
                type_args,
                ..
            } => Some(Self {
                template,
                monomorph: None,
                type_args,
                receiver: None,
                args: &[],
            }),
            _ => None,
        }
    }
}

impl Monomorphizer {
    /// The instance the call or function reference `expr` reaches; `None` where
    /// its callee is emitted as written or its arguments leave the template open.
    pub(super) fn call_instance(
        &self,
        expr: &TirExpr,
        templates: &Templates,
        type_table: &mut TypeTable,
    ) -> Option<InstantiationKey> {
        let site = CallSite::of(expr)?;
        let template = templates.get(site.template)?.borrow();
        if site
            .receiver
            .is_some_and(|r| type_table.receiver_head_awaits_substitution(type_table.peel_refs(r)))
        {
            return None;
        }
        let impl_type_args = if template.impl_type_params.is_empty() {
            Vec::new()
        } else {
            self.impl_args_at(&template, site.template, &site, type_table)?
        };
        let scalar_params = template
            .impl_type_params
            .iter()
            .filter(|param| !param.is_pack)
            .count();
        if impl_type_args.len() < scalar_params {
            return None;
        }
        let method_type_args = method_args_at(&template, &site, type_table);
        if template.has_real_type_params() && method_type_args.is_empty() {
            return None;
        }
        Some(InstantiationKey {
            def: None,
            name: generic_function_name(
                template.is_method(),
                &template.module_source,
                &template.name,
            ),
            module_source: template.module_source.clone(),
            impl_type_args,
            method_type_args,
            method_info: template.method_info.clone(),
            template: Some(site.template.clone()),
        })
    }

    /// The mangled name of the instance `key`, which [`Self::call_instance`]
    /// answered, names.
    pub(super) fn instance_name(
        &self,
        key: &InstantiationKey,
        templates: &Templates,
        type_table: &TypeTable,
    ) -> String {
        let template = key
            .template
            .as_ref()
            .and_then(|id| templates.get(id))
            .expect("an instance names a registered template")
            .borrow();
        self.method_instantiation_name_inner(key, type_table, &template.impl_type_params)
    }

    /// The impl arguments `template` instantiates under at `site`, in the form
    /// its kind keys on: a blanket's own, a tuple's elements, else the head's.
    fn impl_args_at(
        &self,
        template: &TirFunction,
        id: &TemplateId,
        site: &CallSite<'_>,
        type_table: &mut TypeTable,
    ) -> Option<Vec<TypeId>> {
        let recorded = site
            .monomorph
            .map(|m| m.impl_type_args.as_slice())
            .filter(|args| !args.is_empty());
        let blanket = match id {
            TemplateId::Declared {
                block: Some(block), ..
            } => self.functions.trait_env.blanket_of_block(*block).cloned(),
            TemplateId::Declared { block: None, .. } | TemplateId::Synthesized { .. } => None,
        };
        if let Some(blanket) = blanket {
            return self.blanket_args_at(&blanket, site, recorded, type_table);
        }
        let receiver_is_tuple = template
            .method_info
            .as_ref()
            .is_some_and(|info| TypeTable::is_tuple_type(&info.struct_name()));
        if receiver_is_tuple {
            return site
                .receiver
                .and_then(|receiver| type_table.as_tuple(type_table.peel_refs(receiver)))
                .or_else(|| recorded.map(<[TypeId]>::to_vec));
        }
        let Some(receiver) = site.receiver else {
            return recorded.map(<[TypeId]>::to_vec);
        };
        let info = template.method_info.as_ref()?;
        self.struct_info_for_method(
            receiver,
            type_table,
            &info.method_name,
            info.trait_name.as_ref(),
        )
        .map(|(_, args)| args)
    }

    /// A blanket's impl arguments: the receiver it serves, then what its bounds
    /// project off it. A written receiver beats a recorded outer newtype link.
    fn blanket_args_at(
        &self,
        blanket: &BlanketImpl,
        site: &CallSite<'_>,
        recorded: Option<&[TypeId]>,
        type_table: &mut TypeTable,
    ) -> Option<Vec<TypeId>> {
        match blanket.receiver {
            BlanketReceiver::Ref { .. } => {
                recorded
                    .map(<[TypeId]>::to_vec)
                    .or_else(|| match type_table.get(site.receiver?) {
                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                            Some(vec![*inner])
                        }
                        _ => None,
                    })
            }
            BlanketReceiver::Value => {
                let sources = self.functions.trait_env.blanket_param_sources(blanket);
                let slot = sources
                    .iter()
                    .position(|source| matches!(source, BlanketParamSource::Receiver))
                    .unwrap_or(0);
                let from_record = recorded.and_then(|args| match args {
                    [only] => Some(*only),
                    _ => args.get(slot).copied(),
                });
                let subject = match (from_record, site.receiver) {
                    (Some(recorded), Some(receiver))
                        if is_newtype_link_above(
                            recorded,
                            type_table.peel_refs(receiver),
                            type_table,
                        ) =>
                    {
                        type_table.peel_refs(receiver)
                    }
                    (Some(recorded), _) => recorded,
                    (None, Some(receiver)) => type_table.peel_refs(receiver),
                    (None, None) => return None,
                };
                blanket_impl_args(&self.functions.trait_env, blanket, subject, type_table)
                    .or_else(|| recorded.map(<[TypeId]>::to_vec))
            }
        }
    }
}

/// Whether `outer` is a newtype whose chain of bases passes `inner`.
fn is_newtype_link_above(outer: TypeId, inner: TypeId, type_table: &TypeTable) -> bool {
    let mut link = outer;
    while link != inner {
        let ResolvedType::Newtype { base_type, .. } = type_table.get(link) else {
            return false;
        };
        link = *base_type;
    }
    outer != inner
}

/// The method arguments `template` instantiates under at `site`: what the call
/// spells, else what its dispatch recorded, else what its arguments show.
fn method_args_at(
    template: &TirFunction,
    site: &CallSite<'_>,
    type_table: &TypeTable,
) -> Vec<TypeId> {
    if !template.has_real_type_params() {
        return Vec::new();
    }
    if !site.type_args.is_empty() {
        return site.type_args.to_vec();
    }
    if let Some(recorded) = site.monomorph.filter(|m| !m.method_type_args.is_empty()) {
        return recorded.method_type_args.clone();
    }
    let receiver_slots = usize::from(site.receiver.is_some());
    let mut bound: IndexMap<u32, TypeId> = IndexMap::default();
    for (param, arg) in template.params.iter().skip(receiver_slots).zip(site.args) {
        let written = type_table.peel_refs(param.type_id);
        let concrete = type_table.peel_refs(arg.expr.type_id);
        if let Some(slots) = type_table.bind_type_params(&[written], &[concrete]) {
            for (slot, ty) in slots {
                bound.entry(slot).or_insert(ty);
            }
        }
    }
    let offset = template.impl_type_params.len() as u32;
    template
        .type_params
        .iter()
        .filter(|param| !param.is_effect)
        .map(|param| {
            let ty = *bound.get(&(offset + param.index))?;
            Some(type_table.as_box(ty).unwrap_or(ty))
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
}
