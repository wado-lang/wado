//! Synthesis of `From<T>` trait implementations.
//!
//! Handles `impl From<PayloadType> for VariantType;` auto-derive requests.
//! For each request, finds the variant case whose payload matches the source type
//! and generates: `fn from(value: T) -> Self { return Self::CaseName(value); }`

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::name::{FqTypeName, LocalMethodName, MethodName, mangle_generic_name};
use crate::synthesis::common::{
    block, local_ref, make_synthetic_method, param_local, return_stmt, synth_span,
};
use crate::tir::{
    SynthTrait, SynthesisRequest, TirExpr, TirExprKind, TirFunction, TirModule, TirParam, TypeId,
    TypeTable,
};

pub fn synthesize_from(module: &mut TirModule) {
    // Resolve the canonical `From` trait name via the compiler-item registry
    // so a stdlib rename of the trait still routes synthesis through the same
    // anchor. The request carries its source type as a resolved `TypeId`
    // (`SynthTrait::From`), so draining is a structural match — no `From<…>`
    // string parsing.
    let from_trait_name = module
        .type_table
        .borrow()
        .compiler_trait_name(CompilerItem::From)
        .to_string();
    let requests: Vec<SynthesisRequest> = module
        .synthesis_requests
        .extract_if(.., |r| matches!(r.trait_ref, SynthTrait::From { .. }))
        .collect();
    if requests.is_empty() {
        return;
    }

    let existing = collect_existing_from_methods(module, &from_trait_name);
    let mut generated = Vec::new();

    for req in &requests {
        let SynthTrait::From { source } = req.trait_ref else {
            continue;
        };
        // Build the `From<Source>` trait spelling from the source type via the
        // type table — the canonical naming authority — rather than echoing a
        // pre-mangled request string.
        let source_name = module.type_table.borrow().type_name(source);
        let from_trait = mangle_generic_name(&from_trait_name, &[source_name]);
        let key = MethodName::format_local(
            &FqTypeName::declared(&module.module_source, &req.target_type_name),
            Some(&from_trait),
            "from",
        );
        if existing.contains(&key) {
            continue;
        }
        if let Some(func) = generate_variant_from(module, req, source, &from_trait) {
            generated.push(Rc::new(RefCell::new(func)));
        }
    }

    module.functions.extend(generated);
}

fn collect_existing_from_methods(
    module: &TirModule,
    from_trait_name: &str,
) -> crate::hashmap::IndexSet<String> {
    let from_prefix = format!("{from_trait_name}<");
    module
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            func.method_info.as_ref().and_then(|info| {
                info.trait_name.as_ref().and_then(|trait_name| {
                    if trait_name == from_trait_name || trait_name.starts_with(from_prefix.as_str())
                    {
                        Some(MethodName::format_local(
                            &info.fq_base_struct_name(),
                            Some(trait_name),
                            &info.method_name,
                        ))
                    } else {
                        None
                    }
                })
            })
        })
        .collect()
}

fn generate_variant_from(
    module: &TirModule,
    req: &SynthesisRequest,
    source: TypeId,
    from_trait: &str,
) -> Option<TirFunction> {
    let variant_def = module
        .variants
        .iter()
        .find(|v| v.name == req.target_type_name)?;

    // Match the case by resolved type identity, not by type-name string, so two
    // distinct same-named payload types never collide.
    let matching_case = variant_def
        .cases
        .iter()
        .find(|c| c.payload != TypeTable::UNIT && c.payload == source)?;

    let case_name = matching_case.name.clone();
    let case_index = matching_case.index;
    let from_type = matching_case.payload;

    let span = synth_span();
    let variant_type = req.target_type_id;

    let variant_construct = TirExpr::new(
        TirExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload: Some(Box::new(local_ref(0, "value", from_type))),
        },
        variant_type,
        span,
    );

    let body = block(vec![return_stmt(Some(variant_construct))]);
    let locals = vec![param_local("value", from_type, false)];

    let target = FqTypeName::declared(&module.module_source, &req.target_type_name);
    let method_info = LocalMethodName::new(
        target.clone(),
        Some(from_trait.to_string()),
        "from".to_string(),
    );
    let qualified_name = MethodName::format_local(&target, Some(from_trait), "from");

    Some(make_synthetic_method(
        qualified_name,
        method_info,
        vec![TirParam {
            name: "value".to_string(),
            type_id: from_type,
            local_index: 0,
            is_mut: false,
            is_mut_ref: false,
            span,
        }],
        variant_type,
        body,
        locals,
    ))
}
