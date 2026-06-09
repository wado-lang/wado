//! Synthesis of `From<T>` trait implementations.
//!
//! Handles `impl From<PayloadType> for VariantType;` auto-derive requests.
//! For each request, finds the variant case whose payload matches the source type
//! and generates: `fn from(value: T) -> Self { return Self::CaseName(value); }`

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::name::{LocalMethodName, MethodName};
use crate::synthesis::common::{
    block, local_ref, make_synthetic_method, param_local, return_stmt, synth_span,
};
use crate::tir::{
    SynthesisRequest, TirExpr, TirExprKind, TirFunction, TirModule, TirParam, TypeTable,
};

pub fn synthesize_from(module: &mut TirModule) {
    // Resolve the canonical `From` trait name via the compiler-item
    // registry so a stdlib rename of the trait still routes synthesis
    // through the same anchor. The `From<T>` mangled form keeps the
    // registered name as its prefix; build the prefix from the
    // registry-driven name and drain matching requests in one pass.
    let from_trait_name = module
        .type_table
        .borrow()
        .compiler_items()
        .trait_name(CompilerItem::From)
        .to_string();
    let from_prefix = format!("{from_trait_name}<");
    let requests: Vec<SynthesisRequest> = module
        .synthesis_requests
        .extract_if(.., |r| r.trait_name.starts_with(from_prefix.as_str()))
        .collect();
    if requests.is_empty() {
        return;
    }

    let existing = collect_existing_from_methods(module, &from_trait_name);
    let mut generated = Vec::new();

    for req in &requests {
        let from_type_name = extract_from_type_name(&req.trait_name, &from_prefix);
        let from_trait = format!("{from_trait_name}<{from_type_name}>");
        let key = MethodName::format_local(&req.target_type_name, Some(&from_trait), "from");
        if existing.contains(&key) {
            continue;
        }
        if let Some(func) = generate_variant_from(module, req, &from_type_name) {
            generated.push(Rc::new(RefCell::new(func)));
        }
    }

    module.functions.extend(generated);
}

fn extract_from_type_name(trait_name: &str, from_prefix: &str) -> String {
    trait_name
        .strip_prefix(from_prefix)
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trait_name)
        .to_string()
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
                            &info.base_struct_name,
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
    from_type_name: &str,
) -> Option<TirFunction> {
    let variant_def = module
        .variants
        .iter()
        .find(|v| v.name == req.target_type_name)?;

    let tt = module.type_table.borrow();

    let matching_case = variant_def
        .cases
        .iter()
        .find(|c| c.payload != TypeTable::UNIT && tt.type_name(c.payload) == from_type_name)?;

    let case_name = matching_case.name.clone();
    let case_index = matching_case.index;
    let from_type = matching_case.payload;
    drop(tt);

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

    let from_trait_with_arg = format!("From<{from_type_name}>");
    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some(from_trait_with_arg.clone()),
        "from".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some(&from_trait_with_arg), "from");

    Some(make_synthetic_method(
        qualified_name,
        method_info,
        vec![TirParam {
            name: "value".to_string(),
            type_id: from_type,
            local_index: 0,
            is_mut: false,
            span,
        }],
        variant_type,
        body,
        locals,
    ))
}
