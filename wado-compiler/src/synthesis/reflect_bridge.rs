//! Post-monomorphize reflect bridge synthesis.
//!
//! Every other synthesis phase runs before monomorphization. The reflect value
//! bridges cannot: lowering names each one after the *concrete* subject and
//! member type, and members sharing a mangled member type share one
//! index-dispatched bridge. For a generic type that grouping is knowable only
//! per instantiation — `Pair<i32>` merges `left: T` with `right: i32` where
//! `Pair<String>` keeps them apart — and the two call sites are
//! indistinguishable, so a single generic bridge could not be selected
//! (WEP 2026-06-13).
//!
//! The two kinds are found differently. A generic struct is instantiated into
//! its own monomorphized declaration, so its instances are read off the
//! declaration list. A generic variant never is (WEP 2026-02-09), so its
//! instances are read off the type table.

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
    let targets: Vec<(usize, String)> = {
        let tt = flat.type_table.borrow();
        flat.structs
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let base = &s.monomorph_info.as_ref()?.generic_name;
                let base_decl = tt.decl_by_name(base, &s.module_source)?;
                tt.has_generic_assoc_type_def_for_decl(base_decl, REFLECT_MEMBERS_ASSOC)
                    .then(|| (i, base.clone()))
            })
            .collect()
    };

    for (index, base_name) in targets {
        let decl = &flat.structs[index];
        let module_source = decl.module_source.clone();
        let fields: Vec<(String, TypeId, u32)> = decl
            .fields
            .iter()
            .map(|f| (f.name.clone(), f.type_id, f.index))
            .collect();
        let (subject, ref_subject) = {
            let mut tt = flat.type_table.borrow_mut();
            let subject =
                tt.make_monomorphized_struct(decl.name.clone(), module_source.clone(), base_name);
            let ref_subject = tt.make_ref(subject);
            (subject, ref_subject)
        };
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
    // The subject's mangle is the bridge's identity, so a type interned more
    // than once collapses onto one helper rather than minting a duplicate.
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
                // A bridge is minted per *concrete* instantiation. An
                // unsubstituted parameter or projection prints as itself
                // (`Option<V>`, `Option<I::Item>`), so instances from different
                // callers would collide on one name.
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
        // The tag read is named after the subject, so an instantiated generic
        // variant needs its own `discriminant`: the generic declaration's is a
        // template that nothing instantiates, because lowering mints this call
        // after monomorphization has finished.
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
