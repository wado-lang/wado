//! Post-monomorphize reflect bridge synthesis.
//!
//! The one synthesis phase that runs after monomorphize: lowering names a value
//! bridge after the *concrete* subject and member type, and members sharing a
//! mangled member type share one index-dispatched bridge, so a generic type's
//! bridges exist only per instantiation (WEP 2026-06-13).

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TirFunction, TypeId};

use super::traits::{
    REFLECT_MEMBERS_ASSOC, generate_case_bridge_helpers, generate_field_bridge_helpers,
    generate_variant_instance_discriminant_fn,
};

/// Mint the value bridges of every instantiated generic struct and variant whose
/// base carries a synthesized reflect impl.
pub fn synthesize_monomorphized_reflect_bridges(flat: &mut FlatPackage) {
    let mut generated = Vec::new();
    collect_struct_bridges(flat, &mut generated);
    collect_variant_bridges(flat, &mut generated);
    flat.functions.extend(generated);
}

/// `$field_get$S$F` for each monomorphized struct instantiated from a
/// `ReflectStruct`-derived generic base.
fn collect_struct_bridges(flat: &FlatPackage, generated: &mut Vec<Rc<RefCell<TirFunction>>>) {
    let targets: Vec<(usize, String, Vec<TypeId>)> = {
        let tt = flat.type_table.borrow();
        flat.structs
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let mono = s.monomorph_info.as_ref()?;
                let base = &mono.generic_name;
                let base_decl = tt.decl_by_name(base, &s.module_source)?;
                tt.has_generic_assoc_type_def_for_decl(base_decl, REFLECT_MEMBERS_ASSOC)
                    .then(|| (i, base.clone(), mono.impl_type_args.clone()))
            })
            .collect()
    };

    // The subject is the bridge's identity, so a type reached twice — the same
    // instantiation appearing more than once in `flat.structs` — collapses onto
    // one helper, as the variant path already does.
    // Keyed on the subject's mangle, not its `TypeId`: the mangle is what the
    // helper is named after, so two ids that spell the same way would still
    // produce one name twice. The variant path keys the same way.
    let mut seen_subjects: crate::hashmap::IndexSet<String> = crate::hashmap::IndexSet::default();
    for (index, base_name, impl_type_args) in targets {
        let decl = &flat.structs[index];
        let module_source = decl.module_source.clone();
        let fields: Vec<(String, TypeId, u32)> = decl
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.type_id, f.index))
            .collect();
        let (subject, ref_subject) = {
            let mut tt = flat.type_table.borrow_mut();
            // `decl` is an instantiation, so `decl.name` is the rendered name
            // the struct registry keys on. Minting is the unreached branch, and
            // it must rebuild the instantiation from its base and arguments —
            // `make_struct(decl.name)` would register the rendered spelling as a
            // declaration name, the fusion WEP 2026-07-19 removes.
            let subject = tt.find_struct_by_name(&decl.name, &module_source).unwrap_or_else(
                || {
                    tt.make_monomorphized_struct(
                        decl.name.clone(),
                        module_source.clone(),
                        base_name.clone(),
                        impl_type_args.clone(),
                    )
                },
            );
            let ref_subject = tt.make_ref(subject);
            (subject, ref_subject)
        };
        let subject_mangle = flat.type_table.borrow().mangle_type_arg_for_generic(subject);
        if !seen_subjects.insert(subject_mangle) {
            continue;
        }
        push_helpers(
            generate_field_bridge_helpers(
                &flat.type_table,
                &fields,
                subject,
                ref_subject,
                decl.span,
            ),
            &module_source,
            generated,
        );
    }
}

/// `$case_extract$V$P` / `$case_construct$V$P` for each instantiation of a
/// `ReflectVariant`-derived generic variant. A generic variant keeps a single
/// declaration, so the instantiations are the `GenericInstance` types naming it.
fn collect_variant_bridges(flat: &FlatPackage, generated: &mut Vec<Rc<RefCell<TirFunction>>>) {
    // The subject's mangle is the bridge's identity, so a type interned twice
    // collapses onto one helper.
    let mut seen_subjects: crate::hashmap::IndexSet<String> = crate::hashmap::IndexSet::default();
    let instances: Vec<(TypeId, String, ModuleSource, Vec<TypeId>)> = {
        let tt = flat.type_table.borrow();
        tt.iter_type_ids()
            .filter_map(|id| {
                let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } = tt.get(id)
                else {
                    return None;
                };
                // An unsubstituted parameter or projection prints as itself
                // (`Option<V>`), so instances from different callers would
                // collide on one bridge name.
                let concrete = !type_args.iter().any(|&arg| {
                    tt.contains_type_param(arg) || tt.contains_assoc_type_projection(arg)
                });
                if !concrete || !tt.has_generic_assoc_type_def(id, REFLECT_MEMBERS_ASSOC) {
                    return None;
                }
                seen_subjects
                    .insert(tt.mangle_type_arg_for_generic(id))
                    .then(|| (id, name.clone(), module_source.clone(), type_args.clone()))
            })
            .collect()
    };

    for (subject, name, module_source, type_args) in instances {
        let Some(decl) = flat
            .variants
            .iter()
            .find(|v| v.name == name && v.module_source == module_source)
        else {
            continue;
        };
        let substitution: IndexMap<u32, TypeId> = decl
            .type_params
            .iter()
            .zip(&type_args)
            .map(|(param, &arg)| (param.index, arg))
            .collect();
        if substitution.is_empty() {
            continue;
        }
        let span = decl.span;
        let cases: Vec<(String, u32, TypeId, Option<String>)> = {
            let mut tt = flat.type_table.borrow_mut();
            decl.cases
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        c.index,
                        tt.substitute_type_params(c.payload, &substitution),
                        c.wire_name_override.clone(),
                    )
                })
                .collect()
        };
        let ref_subject = flat.type_table.borrow_mut().make_ref(subject);
        let mut helpers =
            generate_case_bridge_helpers(&flat.type_table, &cases, subject, ref_subject, span);
        // The tag read is named after the subject, and lowering mints the call
        // after monomorphization, so the generic declaration's `discriminant`
        // is a template nothing instantiates.
        let discriminant_name = crate::name::variant_tag_helper_name(
            &flat.type_table.borrow().mangle_type_arg_erased(subject),
        );
        helpers.push(generate_variant_instance_discriminant_fn(
            discriminant_name,
            ref_subject,
            subject,
            span,
        ));
        push_helpers(helpers, &module_source, generated);
    }
}

/// `link` — which assigns each synthesized function its home module — has
/// already run by this phase, so the module is set here instead.
fn push_helpers(
    helpers: Vec<TirFunction>,
    module_source: &ModuleSource,
    generated: &mut Vec<Rc<RefCell<TirFunction>>>,
) {
    for mut helper in helpers {
        helper.module_source = module_source.clone();
        generated.push(Rc::new(RefCell::new(helper)));
    }
}
