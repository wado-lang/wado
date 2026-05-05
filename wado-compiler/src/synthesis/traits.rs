//! Trait synthesis phase.
//!
//! Generates auto-derived trait implementations for types that support them:
//! - `EnumName^Eq::eq(&self, &Self) -> bool` - discriminant equality
//! - `EnumName^Ord::cmp(&self, &Self) -> Ordering` - discriminant ordering
//! - `VariantName^Eq::eq(&self, &Self) -> bool` - case-discriminated payload equality
//! - `TypeName^Inspect::inspect(&self, &mut Formatter)` - debug formatting
//! - `TypeName^Display::fmt(&self, &mut Formatter)` - display fallback (delegates to Inspect)
//!
//! Pipeline position: runs as part of the synthesis phase, before monomorphize.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexSet;

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::package::Package;
use crate::tir::{
    CallArg, FnDispatchTrait, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType,
    TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal, TirModule, TirParam,
    TirStmt, TirStmtKind, TirTypeParam, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::common::{
    deref_expr, make_synthetic_method, param_local, ref_expr, synth_span, trait_method_call,
    write_str_stmt,
};

/// Run trait synthesis on the entire project.
///
/// For each module, generates Eq/Ord, Inspect, `InspectAlt`, Display, and `DisplayAlt`
/// implementations for types that don't already have user-provided implementations.
pub fn synthesize_traits(project: Package) -> Package {
    let mut project = project;
    for module in project.tir_modules.values_mut() {
        generate_enum_trait_impls(module);
        generate_struct_eq_ord_impls(module);
        generate_struct_default_impls(module);
        generate_variant_eq_impls(module);
        generate_inspect_impls(module);
        generate_inspect_alt_impls(module);
        generate_display_fallback_impls(module);
        generate_display_alt_fallback_impls(module);
    }
    project
}

/// Check if a trait method implementation already exists for a type.
fn has_existing_impl(
    existing: &IndexSet<String>,
    name: &str,
    trait_name: &str,
    method: &str,
) -> bool {
    existing.contains(&MethodName::format_local(name, Some(trait_name), method))
}

/// Check if a Display/DisplayAlt fallback should be generated.
///
/// Returns true if the display trait doesn't exist yet AND the source inspect trait exists
/// (in either existing trait methods or all function names).
fn should_generate_fallback(
    existing: &IndexSet<String>,
    all_fn_names: &IndexSet<String>,
    name: &str,
    display_trait: &str,
    display_method: &str,
    inspect_trait: &str,
    inspect_method: &str,
) -> bool {
    let display_key = MethodName::format_local(name, Some(display_trait), display_method);
    if existing.contains(&display_key) {
        return false;
    }
    let inspect_key = MethodName::format_local(name, Some(inspect_trait), inspect_method);
    existing.contains(&inspect_key) || all_fn_names.contains(&inspect_key)
}

/// Resolve type parameter definitions into `TypeIds`.
fn make_type_param_ids(type_params: &[TirTypeParam], tt: &mut TypeTable) -> Vec<TypeId> {
    type_params
        .iter()
        .map(|tp| tt.make_type_param(tp.name.clone(), tp.index))
        .collect()
}

type FieldInfo = (String, TypeId, u32);
type VariantCaseInfo = (String, u32, TypeId);

/// Collect non-generic struct info for trait synthesis.
fn collect_struct_fields(module: &TirModule) -> Vec<(String, Vec<FieldInfo>, Span)> {
    module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            (s.name.clone(), fields, s.span)
        })
        .collect()
}

/// Collect generic struct info for trait synthesis.
fn collect_generic_struct_fields(
    module: &TirModule,
) -> Vec<(String, Vec<TirTypeParam>, Vec<FieldInfo>, Span)> {
    module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            (s.name.clone(), s.type_params.clone(), fields, s.span)
        })
        .collect()
}

/// Collect non-generic variant info for trait synthesis.
fn collect_variant_cases(module: &TirModule) -> Vec<(String, Vec<VariantCaseInfo>, Span)> {
    module
        .variants
        .iter()
        .filter(|v| v.type_params.is_empty())
        .map(|v| {
            let cases = v
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.payload))
                .collect();
            (v.name.clone(), cases, v.span)
        })
        .collect()
}

/// Collect generic variant info for trait synthesis.
fn collect_generic_variant_cases(
    module: &TirModule,
) -> Vec<(String, Vec<TirTypeParam>, Vec<VariantCaseInfo>, Span)> {
    module
        .variants
        .iter()
        .filter(|v| !v.type_params.is_empty())
        .map(|v| {
            let cases = v
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.payload))
                .collect();
            (v.name.clone(), v.type_params.clone(), cases, v.span)
        })
        .collect()
}

/// Collect non-generic struct info for Inspect/InspectAlt synthesis (excludes hidden fields).
fn collect_struct_visible_fields(module: &TirModule) -> Vec<(String, Vec<FieldInfo>, bool, Span)> {
    module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .filter(|f| !f.is_hidden)
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            let has_hidden = s.fields.iter().any(|f| f.is_hidden);
            (s.name.clone(), fields, has_hidden, s.span)
        })
        .collect()
}

/// Collect generic struct info for Inspect/InspectAlt synthesis (excludes hidden fields).
fn collect_generic_struct_visible_fields(
    module: &TirModule,
) -> Vec<(String, Vec<TirTypeParam>, Vec<FieldInfo>, bool, Span)> {
    module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .filter(|f| !f.is_hidden)
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            let has_hidden = s.fields.iter().any(|f| f.is_hidden);
            (
                s.name.clone(),
                s.type_params.clone(),
                fields,
                has_hidden,
                s.span,
            )
        })
        .collect()
}

/// Collect existing trait method keys from a module's functions.
fn collect_existing_trait_methods(module: &TirModule) -> IndexSet<String> {
    module
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            func.method_info.as_ref().and_then(|info| {
                info.trait_name.as_ref().map(|trait_name| {
                    format!(
                        "{}^{}::{}",
                        info.base_struct_name, trait_name, info.method_name
                    )
                })
            })
        })
        .collect()
}

/// Generate auto-derived trait implementations (Eq, Ord) for enum types in a module.
fn generate_enum_trait_impls(module: &mut TirModule) {
    if module.enums.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();

    let enum_infos: Vec<_> = module
        .enums
        .iter()
        .map(|e| (e.name.clone(), e.span))
        .collect();

    let existing_trait_methods = collect_existing_trait_methods(module);

    let mut generated_functions = Vec::new();

    for (enum_name, span) in &enum_infos {
        let mut type_table = module.type_table.borrow_mut();
        let enum_type = type_table.make_enum(enum_name.clone(), module_source.clone());
        let ref_enum_type = type_table.make_ref(enum_type);

        if !has_existing_impl(&existing_trait_methods, enum_name, "Eq", "eq") {
            let func = generate_enum_eq_fn(enum_name, enum_type, ref_enum_type, *span);
            generated_functions.push(Rc::new(RefCell::new(func)));
        }

        if !has_existing_impl(&existing_trait_methods, enum_name, "Ord", "cmp") {
            let ordering_type =
                type_table.make_enum("Ordering".to_string(), ModuleSource::traits());
            let func =
                generate_enum_ord_fn(enum_name, enum_type, ref_enum_type, ordering_type, *span);
            generated_functions.push(Rc::new(RefCell::new(func)));
        }
    }

    module.functions.extend(generated_functions);
}

/// Generate auto-derived Eq and Ord trait implementations for struct types in a module.
///
/// For each struct whose fields all support Eq/Ord, generates:
/// - `StructName^Eq::eq(&self, &Self) -> bool` — field-wise equality
/// - `StructName^Ord::cmp(&self, &Self) -> Ordering` — lexicographic field comparison
///
/// Skips structs that already have user-provided implementations.
fn generate_struct_eq_ord_impls(module: &mut TirModule) {
    if module.structs.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let existing = collect_existing_trait_methods(module);
    let mut generated = Vec::new();

    let mut tt = module.type_table.borrow_mut();

    let struct_infos = collect_struct_fields(module);

    for (name, fields, span) in &struct_infos {
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_struct_type = tt.make_ref(struct_type);

        if !has_existing_impl(&existing, name, "Eq", "eq") {
            let func = generate_struct_eq_fn(
                name,
                fields,
                ref_struct_type,
                &module_source,
                &mut tt,
                *span,
            );
            generated.push(Rc::new(RefCell::new(func)));
        }

        if !has_existing_impl(&existing, name, "Ord", "cmp") {
            let ordering_type = tt.make_enum("Ordering".to_string(), ModuleSource::traits());
            let func = generate_struct_ord_fn(
                name,
                fields,
                ref_struct_type,
                ordering_type,
                &module_source,
                &mut tt,
                *span,
            );
            generated.push(Rc::new(RefCell::new(func)));
        }
    }

    let generic_struct_infos = collect_generic_struct_fields(module);

    for (name, type_params, fields, span) in &generic_struct_infos {
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_struct_type = tt.make_ref(struct_type);

        if !has_existing_impl(&existing, name, "Eq", "eq") {
            let func = generate_generic_struct_eq_fn(
                name,
                type_params,
                fields,
                ref_struct_type,
                &module_source,
                &mut tt,
                *span,
            );
            generated.push(Rc::new(RefCell::new(func)));
        }

        if !has_existing_impl(&existing, name, "Ord", "cmp") {
            let ordering_type = tt.make_enum("Ordering".to_string(), ModuleSource::traits());
            let func = generate_generic_struct_ord_fn(
                name,
                type_params,
                fields,
                ref_struct_type,
                ordering_type,
                &module_source,
                &mut tt,
                *span,
            );
            generated.push(Rc::new(RefCell::new(func)));
        }
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate auto-derived `Default` trait implementations for structs whose
/// fields all carry a declared default expression.
///
/// For a non-generic struct `S { f0: T0 = e0, f1: T1 = e1, ... }`, synthesize:
/// - `S^Default::default() -> S` — returns `S { f0: e0, f1: e1, ... }`.
///
/// Skips:
/// - structs where any field has no default expression,
/// - structs that already have a user-provided `impl Default for S`,
/// - generic structs (generic field defaults may depend on bounds; left for
///   a follow-up — monomorphized instances never hit this pass because
///   `monomorph_info.is_some()`).
///
/// Effect purity of the default expressions is already enforced by
/// `check_default_purity` before synthesis runs; if it had failed the pipeline
/// would have bailed, so every `default_expr` reaching here is pure.
fn generate_struct_default_impls(module: &mut TirModule) {
    if module.structs.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let existing = collect_existing_trait_methods(module);
    let mut generated = Vec::new();

    let mut tt = module.type_table.borrow_mut();

    let infos: Vec<(String, Vec<(String, TypeId, u32, TirExpr)>, Span)> = module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .filter_map(|s| {
            let fields: Option<Vec<_>> = s
                .fields
                .iter()
                .map(|f| {
                    f.default_expr
                        .as_ref()
                        .map(|e| (f.name.clone(), f.type_id, f.index, (**e).clone()))
                })
                .collect();
            fields.map(|fields| (s.name.clone(), fields, s.span))
        })
        .collect();

    for (name, fields, span) in &infos {
        if has_existing_impl(&existing, name, "Default", "default") {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let func = generate_struct_default_fn(name, fields, struct_type, *span);
        generated.push(Rc::new(RefCell::new(func)));
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate `StructName^Default::default() -> StructName` for a non-generic
/// struct whose fields all have default expressions.
fn generate_struct_default_fn(
    struct_name: &str,
    fields: &[(String, TypeId, u32, TirExpr)],
    struct_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("Default".to_string()),
        "default".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let struct_fields = fields
        .iter()
        .map(|(name, _type, index, value)| crate::tir::TirStructField {
            name: name.clone(),
            value: value.clone(),
            field_index: *index,
        })
        .collect();

    let literal = TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type,
            struct_name: struct_name.to_string(),
            fields: struct_fields,
        },
        struct_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(literal),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![],
        struct_type,
        body,
        vec![],
    )
}

/// Generate auto-derived Eq trait implementations for variant types in a module.
///
/// For each variant whose payload types all support Eq, generates:
/// - `VariantName^Eq::eq(&self, &Self) -> bool` — case-discriminated payload equality
///
/// Skips variants that already have user-provided implementations.
fn generate_variant_eq_impls(module: &mut TirModule) {
    if module.variants.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let existing = collect_existing_trait_methods(module);
    let mut generated = Vec::new();

    let mut tt = module.type_table.borrow_mut();

    // Non-generic variants
    let variant_infos = collect_variant_cases(module);

    for (name, cases, span) in &variant_infos {
        if has_existing_impl(&existing, name, "Eq", "eq") {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_variant_type = tt.make_ref(variant_type);
        let func = generate_variant_eq_fn(
            name,
            cases,
            variant_type,
            ref_variant_type,
            &module_source,
            &mut tt,
            *span,
        );
        generated.push(Rc::new(RefCell::new(func)));
    }

    // Generic variants
    let generic_variant_infos = collect_generic_variant_cases(module);

    for (name, type_params, cases, span) in &generic_variant_infos {
        if has_existing_impl(&existing, name, "Eq", "eq") {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_variant_type = tt.make_ref(variant_type);
        let func = generate_generic_variant_eq_fn(
            name,
            type_params,
            cases,
            variant_type,
            ref_variant_type,
            &module_source,
            &mut tt,
            *span,
        );
        generated.push(Rc::new(RefCell::new(func)));
    }

    drop(tt);
    module.functions.extend(generated);
}

// ─── Inspect synthesis ───

/// Generate auto-derived `Inspect` trait implementations for all types in a module.
///
/// Generates `TypeName^Inspect::inspect(&self, &mut Formatter)` for:
/// - Enums: if-else chain writing type-qualified case names (e.g., `Color::Red`)
/// - Non-generic structs: writes field names and recursively inspects field values
/// - Generic structs: same with `impl_type_params` having Inspect bounds
/// - Non-generic variants: `VariantTest` dispatch with payload inspection
fn generate_inspect_impls(module: &mut TirModule) {
    let module_source = module.module_source.clone();
    let existing = collect_existing_trait_methods(module);
    let mut generated = Vec::new();

    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct("Formatter".to_string(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());

    // Enums
    let enum_infos: Vec<_> = module
        .enums
        .iter()
        .map(|e| {
            let cases: Vec<_> = e.cases.iter().map(|c| (c.name.clone(), c.index)).collect();
            (e.name.clone(), cases, e.span)
        })
        .collect();

    for (name, cases, espan) in &enum_infos {
        if has_existing_impl(&existing, name, "Inspect", "inspect") {
            continue;
        }
        let enum_type = tt.make_enum(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(enum_type);
        generated.push(Rc::new(RefCell::new(generate_enum_inspect_fn(
            name,
            cases,
            enum_type,
            ref_type,
            fmt_type,
            string_type,
            *espan,
        ))));
    }

    // Non-generic structs
    let struct_infos = collect_struct_visible_fields(module);

    for (name, fields, has_hidden, sspan) in &struct_infos {
        if name == "String" || name == "Formatter" {
            continue;
        }
        if has_existing_impl(&existing, name, "Inspect", "inspect") {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(struct_type);
        generated.push(Rc::new(RefCell::new(generate_struct_inspect_fn(
            name,
            fields,
            *has_hidden,
            ref_type,
            fmt_type,
            string_type,
            &module_source,
            &mut tt,
            *sspan,
        ))));
    }

    // Generic structs
    let generic_struct_infos = collect_generic_struct_visible_fields(module);

    for (name, type_params, fields, has_hidden, sspan) in &generic_struct_infos {
        if has_existing_impl(&existing, name, "Inspect", "inspect") {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(struct_type);
        generated.push(Rc::new(RefCell::new(generate_generic_struct_inspect_fn(
            name,
            type_params,
            fields,
            *has_hidden,
            ref_type,
            fmt_type,
            string_type,
            &module_source,
            &mut tt,
            *sspan,
        ))));
    }

    // Non-generic variants
    let variant_infos = collect_variant_cases(module);

    for (name, cases, vspan) in &variant_infos {
        if has_existing_impl(&existing, name, "Inspect", "inspect") {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(variant_type);
        generated.push(Rc::new(RefCell::new(generate_variant_inspect_fn(
            name,
            cases,
            variant_type,
            ref_type,
            fmt_type,
            string_type,
            &module_source,
            &mut tt,
            *vspan,
        ))));
    }

    // Generic variants (e.g., Option<T>, Result<T, E>)
    let generic_variant_infos = collect_generic_variant_cases(module);

    for (name, type_params, cases, vspan) in &generic_variant_infos {
        if has_existing_impl(&existing, name, "Inspect", "inspect") {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(variant_type);
        generated.push(Rc::new(RefCell::new(generate_generic_variant_inspect_fn(
            name,
            type_params,
            cases,
            variant_type,
            ref_type,
            fmt_type,
            string_type,
            &module_source,
            &mut tt,
            *vspan,
        ))));
    }

    // Flags types (newtypes over u32)
    let flags_infos: Vec<_> = module
        .flags
        .iter()
        .map(|f| {
            let members: Vec<_> = f
                .members
                .iter()
                .map(|m| (m.name.clone(), m.bitmask))
                .collect();
            (f.name.clone(), f.type_id, members, f.span)
        })
        .collect();

    for (name, flags_type_id, members, fspan) in &flags_infos {
        if has_existing_impl(&existing, name, "Inspect", "inspect") {
            continue;
        }
        let ref_type = tt.make_ref(*flags_type_id);
        generated.push(Rc::new(RefCell::new(generate_flags_inspect_fn(
            name,
            *flags_type_id,
            members,
            ref_type,
            fmt_type,
            string_type,
            fspan,
        ))));
    }

    // Newtypes (e.g., `type Meters = f64`)
    for nt in &module.newtypes {
        // Skip flags (they have their own Inspect generation above)
        if module.flags.iter().any(|f| f.type_id == nt.type_id) {
            continue;
        }
        if has_existing_impl(&existing, &nt.name, "Inspect", "inspect") {
            continue;
        }
        let base_type = match tt.get(nt.type_id) {
            ResolvedType::Newtype { base_type, .. } => *base_type,
            _ => continue,
        };
        let ref_type = tt.make_ref(nt.type_id);
        generated.push(Rc::new(RefCell::new(generate_newtype_inspect_fn(
            &nt.name,
            nt.type_id,
            base_type,
            ref_type,
            fmt_type,
            string_type,
            &module_source,
            &mut tt,
            synth_span(),
        ))));
    }

    // Parameterized types (tuples, function types)
    let span = synth_span();
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        let mangled = format_parameterized_name(&base_name, &type_arg_names);
        let inspect_key = format!("{mangled}^Inspect::inspect");
        if existing.contains(&inspect_key) {
            continue;
        }
        let ref_type = tt.make_ref(type_id);
        let resolved = tt.get(type_id).clone();
        match resolved {
            ResolvedType::GenericInstance {
                ref name,
                ref module_source,
                ..
            } if TypeTable::is_tuple_type(name, module_source) => {
                // Tuple Inspect is provided by variadic impl in core:prelude/tuple.wado
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                generated.push(Rc::new(RefCell::new(generate_fn_inspect_fn(
                    &type_arg_names,
                    params.len(),
                    return_type,
                    ref_type,
                    fmt_type,
                    span,
                ))));
            }
            _ => {
                // Opaque/resource types (Future, Stream, etc.): write type name as string
                let type_name = tt.type_name(type_id);
                generated.push(Rc::new(RefCell::new(generate_opaque_inspect_fn(
                    &base_name,
                    &type_arg_names,
                    &type_name,
                    ref_type,
                    fmt_type,
                    string_type,
                    span,
                ))));
            }
        }
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate `EnumName^Inspect::inspect(&self, &mut Formatter)`.
///
/// Body: if-else chain matching discriminant to type-qualified case names.
/// ```text
/// if *self == 0 { f.write_str("Color::Red"); }
/// else if *self == 1 { f.write_str("Color::Green"); }
/// ...
/// ```
fn generate_enum_inspect_fn(
    enum_name: &str,
    cases: &[(String, u32)],
    enum_type: TypeId,
    ref_enum_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        enum_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_enum_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    // Build if-else chain from bottom up
    let deref_self = || {
        TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: 0,
                        name: "self".to_string(),
                    },
                    ref_enum_type,
                    span,
                )),
            },
            enum_type,
            span,
        )
    };
    let fmt_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            fmt_type,
            span,
        )
    };

    let mut chain: Option<TirExpr> = None;
    for (case_name, case_index) in cases.iter().rev() {
        let text = format!("{enum_name}::{case_name}");
        let then_block = TirBlock::new(
            vec![write_str_stmt(text, fmt_local(), string_type, span)],
            span,
        );
        let cond = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(deref_self()),
                op: TirBinaryOp::Eq,
                right: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: u64::from(*case_index),
                        repr: case_index.to_string(),
                    },
                    enum_type,
                    span,
                )),
            },
            TypeTable::BOOL,
            span,
        );
        let if_expr = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(cond),
                then_branch: then_block,
                else_branch: chain
                    .map(|e| TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(e), span)], span)),
            },
            TypeTable::UNIT,
            span,
        );
        chain = Some(if_expr);
    }

    let stmts = chain.map_or_else(Vec::new, |e| vec![TirStmt::new(TirStmtKind::Expr(e), span)]);
    let body = TirBlock::new(stmts, span);

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_enum_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Generate `StructName^Inspect::inspect(&self, &mut Formatter)` for non-generic structs.
///
/// Body:
/// ```text
/// f.write_str("StructName { ");
/// f.write_str("field1: "); self.field1.inspect(f);
/// f.write_str(", field2: "); self.field2.inspect(f);
/// f.write_str(" }");
/// ```
fn generate_struct_inspect_fn(
    struct_name: &str,
    fields: &[(String, TypeId, u32)],
    has_hidden: bool,
    ref_struct_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_struct_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let self_ref = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 0,
                name: "self".to_string(),
            },
            ref_struct_type,
            span,
        )
    };
    let fmt_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            fmt_type,
            span,
        )
    };

    let mut stmts = Vec::new();

    if fields.is_empty() {
        if has_hidden {
            stmts.push(write_str_stmt(
                format!("{struct_name} {{ .. }}"),
                fmt_local(),
                string_type,
                span,
            ));
        } else {
            stmts.push(write_str_stmt(
                format!("{struct_name} {{}}"),
                fmt_local(),
                string_type,
                span,
            ));
        }
    } else {
        stmts.push(write_str_stmt(
            format!("{struct_name} {{ "),
            fmt_local(),
            string_type,
            span,
        ));
        for (i, (field_name, field_type, field_index)) in fields.iter().enumerate() {
            if i > 0 {
                stmts.push(write_str_stmt(
                    ", ".to_string(),
                    fmt_local(),
                    string_type,
                    span,
                ));
            }
            stmts.push(write_str_stmt(
                format!("{field_name}: "),
                fmt_local(),
                string_type,
                span,
            ));
            // self.field_name — FieldAccess through &self gives the field value
            let field_access = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(self_ref()),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            stmts.push(inspect_call(
                field_access,
                *field_type,
                fmt_local(),
                module_source,
                tt,
                span,
            ));
        }
        if has_hidden {
            stmts.push(write_str_stmt(
                ", ..".to_string(),
                fmt_local(),
                string_type,
                span,
            ));
        }
        stmts.push(write_str_stmt(
            " }".to_string(),
            fmt_local(),
            string_type,
            span,
        ));
    }

    let body = TirBlock::new(stmts, span);

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_struct_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Generate `StructName^Inspect::inspect(&self, &mut Formatter)` for generic structs.
///
/// Sets `impl_type_params` with Inspect bounds so the monomorphizer can
/// specialize field inspect calls for concrete types.
fn generate_generic_struct_inspect_fn(
    struct_name: &str,
    type_params: &[TirTypeParam],
    fields: &[(String, TypeId, u32)],
    has_hidden: bool,
    ref_struct_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_struct_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let self_ref = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 0,
                name: "self".to_string(),
            },
            ref_struct_type,
            span,
        )
    };
    let fmt_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            fmt_type,
            span,
        )
    };

    let mut stmts = Vec::new();

    if fields.is_empty() {
        if has_hidden {
            stmts.push(write_str_stmt(
                format!("{struct_name} {{ .. }}"),
                fmt_local(),
                string_type,
                span,
            ));
        } else {
            stmts.push(write_str_stmt(
                format!("{struct_name} {{}}"),
                fmt_local(),
                string_type,
                span,
            ));
        }
    } else {
        stmts.push(write_str_stmt(
            format!("{struct_name} {{ "),
            fmt_local(),
            string_type,
            span,
        ));
        for (i, (field_name, field_type, field_index)) in fields.iter().enumerate() {
            if i > 0 {
                stmts.push(write_str_stmt(
                    ", ".to_string(),
                    fmt_local(),
                    string_type,
                    span,
                ));
            }
            stmts.push(write_str_stmt(
                format!("{field_name}: "),
                fmt_local(),
                string_type,
                span,
            ));
            let field_access = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(self_ref()),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            stmts.push(inspect_call(
                field_access,
                *field_type,
                fmt_local(),
                module_source,
                tt,
                span,
            ));
        }
        if has_hidden {
            stmts.push(write_str_stmt(
                ", ..".to_string(),
                fmt_local(),
                string_type,
                span,
            ));
        }
        stmts.push(write_str_stmt(
            " }".to_string(),
            fmt_local(),
            string_type,
            span,
        ));
    }

    let body = TirBlock::new(stmts, span);

    // impl_type_params: same as the struct's type_params
    // The monomorphizer uses these to specialize the function
    let impl_type_params: Vec<TirTypeParam> = type_params.to_vec();

    let local_count = 2;
    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(method_info),
        params,
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count,
        locals: vec![
            param_local("self", ref_struct_type, false),
            param_local("f", fmt_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

/// Generate `VariantName^Inspect::inspect(&self, &mut Formatter)` for non-generic variants.
///
/// Body: `VariantTest` dispatch with type-qualified case names.
/// ```text
/// if variant_test(self, 0) { f.write_str("Shape::Circle("); payload.inspect(f); f.write_str(")"); }
/// else if variant_test(self, 1) { f.write_str("Shape::Point"); }
/// ...
/// ```
fn generate_variant_inspect_fn(
    variant_name: &str,
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        variant_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_variant_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let deref_self = || {
        TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: 0,
                        name: "self".to_string(),
                    },
                    ref_variant_type,
                    span,
                )),
            },
            variant_type,
            span,
        )
    };
    let fmt_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            fmt_type,
            span,
        )
    };

    let mut chain: Option<TirExpr> = None;
    for (case_name, case_index, payload_type) in cases.iter().rev() {
        let is_unit = *payload_type == TypeTable::UNIT;
        let mut then_stmts = Vec::new();

        if is_unit {
            then_stmts.push(write_str_stmt(
                format!("{variant_name}::{case_name}"),
                fmt_local(),
                string_type,
                span,
            ));
        } else {
            then_stmts.push(write_str_stmt(
                format!("{variant_name}::{case_name}("),
                fmt_local(),
                string_type,
                span,
            ));
            let payload = TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(deref_self()),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                *payload_type,
                span,
            );
            then_stmts.push(inspect_call(
                payload,
                *payload_type,
                fmt_local(),
                module_source,
                tt,
                span,
            ));
            then_stmts.push(write_str_stmt(
                ")".to_string(),
                fmt_local(),
                string_type,
                span,
            ));
        }

        let cond = TirExpr::new(
            TirExprKind::VariantTest {
                expr: Box::new(deref_self()),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TypeTable::BOOL,
            span,
        );
        let if_expr = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(cond),
                then_branch: TirBlock::new(then_stmts, span),
                else_branch: chain
                    .map(|e| TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(e), span)], span)),
            },
            TypeTable::UNIT,
            span,
        );
        chain = Some(if_expr);
    }

    let stmts = chain.map_or_else(Vec::new, |e| vec![TirStmt::new(TirStmtKind::Expr(e), span)]);
    let body = TirBlock::new(stmts, span);

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_variant_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Generate `VariantName^Inspect::inspect(&self, &mut Formatter)` for generic variants.
///
/// Same structure as `generate_variant_inspect_fn` but with `impl_type_params` set so the
/// monomorphizer can specialize it for each concrete instantiation (e.g. `Option<i32>`).
fn generate_generic_variant_inspect_fn(
    variant_name: &str,
    type_params: &[TirTypeParam],
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        variant_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_variant_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let deref_self = || {
        TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: 0,
                        name: "self".to_string(),
                    },
                    ref_variant_type,
                    span,
                )),
            },
            variant_type,
            span,
        )
    };
    let fmt_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            fmt_type,
            span,
        )
    };

    let mut chain: Option<TirExpr> = None;
    for (case_name, case_index, payload_type) in cases.iter().rev() {
        let is_unit = *payload_type == TypeTable::UNIT;
        let mut then_stmts = Vec::new();

        if is_unit {
            then_stmts.push(write_str_stmt(
                format!("{variant_name}::{case_name}"),
                fmt_local(),
                string_type,
                span,
            ));
        } else {
            then_stmts.push(write_str_stmt(
                format!("{variant_name}::{case_name}("),
                fmt_local(),
                string_type,
                span,
            ));
            let payload = TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(deref_self()),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                *payload_type,
                span,
            );
            then_stmts.push(inspect_call(
                payload,
                *payload_type,
                fmt_local(),
                module_source,
                tt,
                span,
            ));
            then_stmts.push(write_str_stmt(
                ")".to_string(),
                fmt_local(),
                string_type,
                span,
            ));
        }

        let cond = TirExpr::new(
            TirExprKind::VariantTest {
                expr: Box::new(deref_self()),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TypeTable::BOOL,
            span,
        );
        let if_expr = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(cond),
                then_branch: TirBlock::new(then_stmts, span),
                else_branch: chain
                    .map(|e| TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(e), span)], span)),
            },
            TypeTable::UNIT,
            span,
        );
        chain = Some(if_expr);
    }

    let stmts = chain.map_or_else(Vec::new, |e| vec![TirStmt::new(TirStmtKind::Expr(e), span)]);
    let body = TirBlock::new(stmts, span);

    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: type_params.to_vec(),
        monomorph_info: None,
        method_info: Some(method_info),
        params,
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: 2,
        locals: vec![
            param_local("self", ref_variant_type, false),
            param_local("f", fmt_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

/// Generate `NewtypeName^Inspect::inspect(&self, &mut Formatter)` for a newtype.
///
/// Body: inspects the base type value, then writes ` as NewtypeName`.
/// e.g., `100.5 as Meters`
fn generate_newtype_inspect_fn(
    newtype_name: &str,
    newtype_type: TypeId,
    base_type: TypeId,
    ref_newtype_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        newtype_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let deref_self = || {
        deref_expr(
            TirExpr::new(
                TirExprKind::Local {
                    index: 0,
                    name: "self".to_string(),
                },
                ref_newtype_type,
                span,
            ),
            newtype_type,
            span,
        )
    };
    let fmt_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            fmt_type,
            span,
        )
    };

    // Cast to base type
    let cast_to_base = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(deref_self()),
            target_type: base_type,
        },
        base_type,
        span,
    );

    let mut stmts = Vec::new();
    // Inspect the base value
    stmts.push(inspect_call(
        cast_to_base,
        base_type,
        fmt_local(),
        module_source,
        tt,
        span,
    ));
    // Write " as NewtypeName"
    stmts.push(write_str_stmt(
        format!(" as {newtype_name}"),
        fmt_local(),
        string_type,
        span,
    ));

    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_newtype_type, fmt_type, span),
        TypeTable::UNIT,
        TirBlock::new(stmts, span),
        vec![
            param_local("self", ref_newtype_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Generate `FlagsName^Inspect::inspect(&self, &mut Formatter)` for a flags type.
///
/// Body: checks each member bit and writes `FlagsName::Member1 | FlagsName::Member2`,
/// or `FlagsName::none()` if no bits are set.
fn generate_flags_inspect_fn(
    flags_name: &str,
    flags_type: TypeId,
    members: &[(String, u32)],
    ref_flags_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    span: &Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        flags_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let deref_self = || {
        TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: 0,
                        name: "self".to_string(),
                    },
                    ref_flags_type,
                    *span,
                )),
            },
            flags_type,
            *span,
        )
    };
    // Cast deref'd flags value to u32 for bit operations
    let self_as_u32 = || {
        TirExpr::new(
            TirExprKind::Cast {
                expr: Box::new(deref_self()),
                target_type: TypeTable::U32,
            },
            TypeTable::U32,
            *span,
        )
    };
    let fmt_local = || {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "f".to_string(),
            },
            fmt_type,
            *span,
        )
    };

    let mut stmts = Vec::new();

    // if (self as u32) == 0 { f.write_str("FlagsName::none()"); }
    let zero_cond = TirExpr::new(
        TirExprKind::Binary {
            op: TirBinaryOp::Eq,
            left: Box::new(self_as_u32()),
            right: Box::new(TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".to_string(),
                },
                TypeTable::U32,
                *span,
            )),
        },
        TypeTable::BOOL,
        *span,
    );
    let zero_branch = TirExpr::new(
        TirExprKind::If {
            condition: Box::new(zero_cond),
            then_branch: TirBlock::new(
                vec![write_str_stmt(
                    format!("{flags_name}::none()"),
                    fmt_local(),
                    string_type,
                    *span,
                )],
                *span,
            ),
            else_branch: None,
        },
        TypeTable::UNIT,
        *span,
    );
    stmts.push(TirStmt::new(TirStmtKind::Expr(zero_branch), *span));

    // For each member: if (self as u32) & bitmask != 0 { ... }
    let mut mask_below: u32 = 0;
    for (member_name, bitmask) in members {
        // Check bit: (self as u32) & bitmask != 0
        let bit_check = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::NotEq,
                left: Box::new(TirExpr::new(
                    TirExprKind::Binary {
                        op: TirBinaryOp::BitAnd,
                        left: Box::new(self_as_u32()),
                        right: Box::new(TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: u64::from(*bitmask),
                                repr: bitmask.to_string(),
                            },
                            TypeTable::U32,
                            *span,
                        )),
                    },
                    TypeTable::U32,
                    *span,
                )),
                right: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: 0,
                        repr: "0".to_string(),
                    },
                    TypeTable::U32,
                    *span,
                )),
            },
            TypeTable::BOOL,
            *span,
        );

        let mut then_stmts = Vec::new();

        // Write separator if any previous bits were set
        if mask_below != 0 {
            let sep_cond = TirExpr::new(
                TirExprKind::Binary {
                    op: TirBinaryOp::NotEq,
                    left: Box::new(TirExpr::new(
                        TirExprKind::Binary {
                            op: TirBinaryOp::BitAnd,
                            left: Box::new(self_as_u32()),
                            right: Box::new(TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: u64::from(mask_below),
                                    repr: mask_below.to_string(),
                                },
                                TypeTable::U32,
                                *span,
                            )),
                        },
                        TypeTable::U32,
                        *span,
                    )),
                    right: Box::new(TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: "0".to_string(),
                        },
                        TypeTable::U32,
                        *span,
                    )),
                },
                TypeTable::BOOL,
                *span,
            );
            let sep_if = TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(sep_cond),
                    then_branch: TirBlock::new(
                        vec![write_str_stmt(" | ", fmt_local(), string_type, *span)],
                        *span,
                    ),
                    else_branch: None,
                },
                TypeTable::UNIT,
                *span,
            );
            then_stmts.push(TirStmt::new(TirStmtKind::Expr(sep_if), *span));
        }

        then_stmts.push(write_str_stmt(
            format!("{flags_name}::{member_name}"),
            fmt_local(),
            string_type,
            *span,
        ));

        let member_if = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(bit_check),
                then_branch: TirBlock::new(then_stmts, *span),
                else_branch: None,
            },
            TypeTable::UNIT,
            *span,
        );
        stmts.push(TirStmt::new(TirStmtKind::Expr(member_if), *span));

        mask_below |= bitmask;
    }

    let body = TirBlock::new(stmts, *span);
    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_flags_type, fmt_type, *span),
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_flags_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Generate `Fn<N, Ret>^Inspect::inspect(&self, &mut Formatter)` as
/// an auto-derived dispatch stub.
///
/// The TIR body is an empty unit block — a placeholder that exists
/// only so the function entry is registered (and so call sites
/// referring to it from templates / user code resolve). WIR build
/// recognises [`FunctionKind::FnCanonicalDispatch`] and supplies the
/// real body: a `call_ref` through the matching `CanonicalClosure_K`'s
/// `inspect` vtable slot. See WEP: Inspect (Debug Output) > Closure
/// Inspect via Runtime Dispatch.
fn generate_fn_inspect_fn(
    type_arg_names: &[String],
    arity: usize,
    return_type: TypeId,
    ref_fn_type: TypeId,
    fmt_type: TypeId,
    span: Span,
) -> TirFunction {
    generate_fn_canonical_dispatch_stub(
        FnDispatchTrait::Inspect,
        "Inspect",
        "inspect",
        type_arg_names,
        arity,
        return_type,
        ref_fn_type,
        fmt_type,
        span,
    )
}

/// Twin of [`generate_fn_inspect_fn`] for `Fn<N, Ret>^InspectAlt`.
fn generate_fn_inspect_alt_fn(
    type_arg_names: &[String],
    arity: usize,
    return_type: TypeId,
    ref_fn_type: TypeId,
    fmt_type: TypeId,
    span: Span,
) -> TirFunction {
    generate_fn_canonical_dispatch_stub(
        FnDispatchTrait::InspectAlt,
        "InspectAlt",
        "inspect_alt",
        type_arg_names,
        arity,
        return_type,
        ref_fn_type,
        fmt_type,
        span,
    )
}

/// Shared body for [`generate_fn_inspect_fn`] /
/// [`generate_fn_inspect_alt_fn`]. The two only differ in the trait
/// label and the `FnDispatchTrait` carried in `FunctionKind`.
#[allow(clippy::too_many_arguments)]
fn generate_fn_canonical_dispatch_stub(
    trait_kind: FnDispatchTrait,
    trait_name: &str,
    method_name: &str,
    type_arg_names: &[String],
    arity: usize,
    return_type: TypeId,
    ref_fn_type: TypeId,
    fmt_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        "Fn".to_string(),
        Some(trait_name.to_string()),
        method_name.to_string(),
    )
    .with_struct_type_args(type_arg_names);
    let qualified_name = method_info.to_mangled_name();

    // Empty unit body: WIR build supplies the real instructions.
    let body = TirBlock::new(vec![], span);

    let mut func = make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_fn_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_fn_type, false),
            param_local("f", fmt_type, false),
        ],
    );
    func.kind = FunctionKind::FnCanonicalDispatch {
        trait_kind,
        arity,
        return_type,
    };
    // The empty placeholder TIR body is overwritten by WIR build; tell
    // the inliner not to expand the empty body into call sites before
    // that happens (otherwise `:?` / `:#?` would silently produce
    // nothing at higher optimisation levels).
    func.inline_hint = InlineHint::Never;
    func
}

/// Generate Inspect for opaque/resource types (Future, Stream, etc.).
///
/// Body: writes the type name as a static string, e.g., `Future<i32>`.
fn generate_opaque_inspect_fn(
    base_name: &str,
    type_arg_names: &[String],
    type_name: &str,
    ref_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        base_name.to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    )
    .with_struct_type_args(type_arg_names);
    let qualified_name = method_info.to_mangled_name();

    let fmt = || local_expr(1, "f", fmt_type, span);
    let body = TirBlock::new(
        vec![write_str_stmt(
            type_name.to_string(),
            fmt(),
            string_type,
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Standard parameters for `Inspect::inspect` and `Display::fmt`.
fn inspect_params(ref_type: TypeId, fmt_type: TypeId, span: Span) -> Vec<TirParam> {
    vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ]
}

/// Create a local variable reference expression.
fn local_expr(index: u32, name: &str, type_id: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Local {
            index,
            name: name.to_string(),
        },
        type_id,
        span,
    )
}

// ─── InspectAlt synthesis ───

/// Generate auto-derived `InspectAlt` trait implementations for all types in a module.
///
/// For composite types (structs, variants, arrays, tuples), generates pretty-printed
/// multi-line output using Formatter's `begin_block`/`end_block`/`write_field_sep` helpers.
/// For simple types (enums, flags, newtypes, primitives, functions), delegates to Inspect.
fn generate_inspect_alt_impls(module: &mut TirModule) {
    let module_source = module.module_source.clone();
    let existing = collect_existing_trait_methods(module);
    let all_fn_names: IndexSet<String> = module
        .functions
        .iter()
        .filter_map(|f| f.try_borrow().ok().map(|func| func.name.clone()))
        .collect();
    let mut generated = Vec::new();

    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct("Formatter".to_string(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let span = synth_span();

    // Helper: check if InspectAlt should be generated.
    // Returns true if no InspectAlt exists AND an Inspect impl exists.
    let has_inspect = |inspect_key: &str| -> bool {
        existing.contains(inspect_key) || all_fn_names.contains(inspect_key)
    };

    // Enums — delegate to Inspect (no multiline needed for enum names)
    for name in module
        .enums
        .iter()
        .map(|e| e.name.clone())
        .collect::<Vec<_>>()
    {
        if has_existing_impl(&existing, &name, "InspectAlt", "inspect_alt")
            || !has_inspect(&MethodName::format_local(&name, Some("Inspect"), "inspect"))
        {
            continue;
        }
        let enum_type = tt.make_enum(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(enum_type);
        let di = LocalMethodName::new(
            name.clone(),
            Some("InspectAlt".to_string()),
            "inspect_alt".to_string(),
        );
        let ii = LocalMethodName::new(name, Some("Inspect".to_string()), "inspect".to_string());
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Non-generic structs — pretty-print with begin_block/end_block
    let struct_infos = collect_struct_visible_fields(module);

    for (name, fields, has_hidden, sspan) in &struct_infos {
        if name == "String" || name == "Formatter" {
            continue;
        }
        if has_existing_impl(&existing, name, "InspectAlt", "inspect_alt") {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(struct_type);
        generated.push(Rc::new(RefCell::new(generate_struct_inspect_alt_fn(
            name,
            fields,
            *has_hidden,
            ref_type,
            fmt_type,
            string_type,
            &module_source,
            &mut tt,
            *sspan,
        ))));
    }

    // Generic structs — pretty-print with impl_type_params
    let generic_struct_infos = collect_generic_struct_visible_fields(module);

    for (name, type_params, fields, has_hidden, sspan) in &generic_struct_infos {
        if has_existing_impl(&existing, name, "InspectAlt", "inspect_alt") {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(struct_type);
        generated.push(Rc::new(RefCell::new(
            generate_generic_struct_inspect_alt_fn(
                name,
                type_params,
                fields,
                *has_hidden,
                ref_type,
                fmt_type,
                string_type,
                &module_source,
                &mut tt,
                *sspan,
            ),
        )));
    }

    // Non-generic variants — pretty-print
    let variant_infos = collect_variant_cases(module);

    for (name, cases, vspan) in &variant_infos {
        if has_existing_impl(&existing, name, "InspectAlt", "inspect_alt") {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(variant_type);
        generated.push(Rc::new(RefCell::new(generate_variant_inspect_alt_fn(
            name,
            cases,
            variant_type,
            ref_type,
            fmt_type,
            string_type,
            &module_source,
            &mut tt,
            *vspan,
        ))));
    }

    // Generic variants — pretty-print
    let generic_variant_infos = collect_generic_variant_cases(module);

    for (name, type_params, cases, vspan) in &generic_variant_infos {
        if has_existing_impl(&existing, name, "InspectAlt", "inspect_alt") {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(variant_type);
        generated.push(Rc::new(RefCell::new(
            generate_generic_variant_inspect_alt_fn(
                name,
                type_params,
                cases,
                variant_type,
                ref_type,
                fmt_type,
                string_type,
                &module_source,
                &mut tt,
                *vspan,
            ),
        )));
    }

    // Flags — delegate to Inspect (bit flags don't need pretty print)
    let flags_infos: Vec<_> = module
        .flags
        .iter()
        .map(|f| (f.name.clone(), f.type_id))
        .collect();

    for (name, flags_type_id) in &flags_infos {
        if has_existing_impl(&existing, name, "InspectAlt", "inspect_alt")
            || !has_inspect(&MethodName::format_local(name, Some("Inspect"), "inspect"))
        {
            continue;
        }
        let ref_type = tt.make_ref(*flags_type_id);
        let di = LocalMethodName::new(
            name.clone(),
            Some("InspectAlt".to_string()),
            "inspect_alt".to_string(),
        );
        let ii = LocalMethodName::new(
            name.clone(),
            Some("Inspect".to_string()),
            "inspect".to_string(),
        );
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Newtypes — delegate to Inspect
    for nt in &module.newtypes {
        if module.flags.iter().any(|f| f.type_id == nt.type_id) {
            continue;
        }
        if has_existing_impl(&existing, &nt.name, "InspectAlt", "inspect_alt")
            || !has_inspect(&MethodName::format_local(
                &nt.name,
                Some("Inspect"),
                "inspect",
            ))
        {
            continue;
        }
        let ref_type = tt.make_ref(nt.type_id);
        let di = LocalMethodName::new(
            nt.name.clone(),
            Some("InspectAlt".to_string()),
            "inspect_alt".to_string(),
        );
        let ii = LocalMethodName::new(
            nt.name.clone(),
            Some("Inspect".to_string()),
            "inspect".to_string(),
        );
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Parameterized types (tuples, function types)
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        let mangled = format_parameterized_name(&base_name, &type_arg_names);
        let alt_key = format!("{mangled}^InspectAlt::inspect_alt");
        let inspect_key = format!("{mangled}^Inspect::inspect");
        if existing.contains(&alt_key) || !has_inspect(&inspect_key) {
            continue;
        }
        let ref_type = tt.make_ref(type_id);
        let resolved = tt.get(type_id).clone();
        if matches!(resolved, ResolvedType::GenericInstance { ref name, ref module_source, .. } if TypeTable::is_tuple_type(name, module_source))
        {
            // Tuple InspectAlt is provided by variadic impl in core:prelude/tuple.wado
        } else if let ResolvedType::Function {
            params,
            return_type,
            ..
        } = &resolved
        {
            // Function: emit a stand-alone InspectAlt stub. Crucially,
            // do NOT use the `display_fallback` Inspect-delegate: WIR
            // build supplies the real body — `call_ref
            // (self.inspect_alt)` for InspectAlt, `call_ref
            // (self.inspect)` for Inspect — and a delegate would let
            // the optimizer collapse InspectAlt to Inspect before WIR
            // build runs, defeating the per-literal source dispatch.
            generated.push(Rc::new(RefCell::new(generate_fn_inspect_alt_fn(
                &type_arg_names,
                params.len(),
                *return_type,
                ref_type,
                fmt_type,
                span,
            ))));
        } else {
            // Opaque resource types (Future, Stream, etc.): delegate
            // to Inspect (no per-literal data to dispatch through).
            let di = LocalMethodName::new(
                base_name.clone(),
                Some("InspectAlt".to_string()),
                "inspect_alt".to_string(),
            )
            .with_struct_type_args(&type_arg_names);
            let ii = LocalMethodName::new(
                base_name,
                Some("Inspect".to_string()),
                "inspect".to_string(),
            )
            .with_struct_type_args(&type_arg_names);
            generated.push(Rc::new(RefCell::new(generate_display_fallback(
                di,
                ii,
                ref_type,
                fmt_type,
                &module_source,
                vec![],
                span,
            ))));
        }
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate `StructName^InspectAlt::inspect_alt` for non-generic structs (pretty-print).
///
/// Body uses Formatter helpers for clean synthesis:
/// ```text
/// f.begin_block("StructName {\n");
/// f.write_indent(); f.write_str("field1: "); self.field1.inspect_alt(f); f.write_str(",\n");
/// f.write_indent(); f.write_str("field2: "); self.field2.inspect_alt(f); f.write_str(",\n");
/// f.end_block("}");
/// ```
fn generate_struct_inspect_alt_fn(
    struct_name: &str,
    fields: &[(String, TypeId, u32)],
    has_hidden: bool,
    ref_struct_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("InspectAlt".to_string()),
        "inspect_alt".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let self_ref = || local_expr(0, "self", ref_struct_type, span);
    let fmt_local = || local_expr(1, "f", fmt_type, span);

    let stmts = build_struct_inspect_alt_body(
        struct_name,
        fields,
        has_hidden,
        self_ref,
        fmt_local,
        string_type,
        module_source,
        tt,
        span,
    );

    let body = TirBlock::new(stmts, span);
    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_struct_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_struct_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Generate `StructName^InspectAlt::inspect_alt` for generic structs (pretty-print).
fn generate_generic_struct_inspect_alt_fn(
    struct_name: &str,
    type_params: &[TirTypeParam],
    fields: &[(String, TypeId, u32)],
    has_hidden: bool,
    ref_struct_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("InspectAlt".to_string()),
        "inspect_alt".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let self_ref = || local_expr(0, "self", ref_struct_type, span);
    let fmt_local = || local_expr(1, "f", fmt_type, span);

    let stmts = build_struct_inspect_alt_body(
        struct_name,
        fields,
        has_hidden,
        self_ref,
        fmt_local,
        string_type,
        module_source,
        tt,
        span,
    );

    let body = TirBlock::new(stmts, span);
    let impl_type_params: Vec<TirTypeParam> = type_params.to_vec();

    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(method_info),
        params: inspect_params(ref_struct_type, fmt_type, span),
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: 2,
        locals: vec![
            param_local("self", ref_struct_type, false),
            param_local("f", fmt_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

/// Build the body statements for struct `InspectAlt` (shared between generic and non-generic).
fn build_struct_inspect_alt_body(
    struct_name: &str,
    fields: &[(String, TypeId, u32)],
    has_hidden: bool,
    self_ref: impl Fn() -> TirExpr,
    fmt_local: impl Fn() -> TirExpr,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> Vec<TirStmt> {
    let mut stmts = Vec::new();

    if fields.is_empty() {
        if has_hidden {
            stmts.push(write_str_stmt(
                format!("{struct_name} {{ .. }}"),
                fmt_local(),
                string_type,
                span,
            ));
        } else {
            stmts.push(write_str_stmt(
                format!("{struct_name} {{}}"),
                fmt_local(),
                string_type,
                span,
            ));
        }
    } else {
        // f.begin_block("StructName {")
        stmts.push(formatter_call(
            "open_brace",
            fmt_local(),
            Some((format!("{struct_name} {{"), string_type)),
            span,
        ));
        for (field_name, field_type, field_index) in fields {
            // f.write_newline_indent()
            stmts.push(formatter_call(
                "write_newline_indent",
                fmt_local(),
                None::<(&str, TypeId)>,
                span,
            ));
            stmts.push(write_str_stmt(
                format!("{field_name}: "),
                fmt_local(),
                string_type,
                span,
            ));
            let field_access = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(self_ref()),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            stmts.push(inspect_alt_call(
                field_access,
                *field_type,
                fmt_local(),
                module_source,
                tt,
                span,
            ));
            stmts.push(write_str_stmt(",", fmt_local(), string_type, span));
        }
        if has_hidden {
            stmts.push(formatter_call(
                "write_newline_indent",
                fmt_local(),
                None::<(&str, TypeId)>,
                span,
            ));
            stmts.push(write_str_stmt("..", fmt_local(), string_type, span));
        }
        // f.end_block("}")
        stmts.push(formatter_call(
            "close_brace",
            fmt_local(),
            Some(("}", string_type)),
            span,
        ));
    }

    stmts
}

/// Generate `VariantName^InspectAlt::inspect_alt` for non-generic variants (pretty-print).
fn generate_variant_inspect_alt_fn(
    variant_name: &str,
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        variant_name.to_string(),
        Some("InspectAlt".to_string()),
        "inspect_alt".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let stmts = build_variant_inspect_alt_body(
        variant_name,
        cases,
        variant_type,
        ref_variant_type,
        fmt_type,
        string_type,
        module_source,
        tt,
        span,
    );

    let body = TirBlock::new(stmts, span);
    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_variant_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        vec![
            param_local("self", ref_variant_type, false),
            param_local("f", fmt_type, false),
        ],
    )
}

/// Generate `VariantName^InspectAlt::inspect_alt` for generic variants (pretty-print).
fn generate_generic_variant_inspect_alt_fn(
    variant_name: &str,
    type_params: &[TirTypeParam],
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        variant_name.to_string(),
        Some("InspectAlt".to_string()),
        "inspect_alt".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let stmts = build_variant_inspect_alt_body(
        variant_name,
        cases,
        variant_type,
        ref_variant_type,
        fmt_type,
        string_type,
        module_source,
        tt,
        span,
    );

    let body = TirBlock::new(stmts, span);
    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: type_params.to_vec(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: inspect_params(ref_variant_type, fmt_type, span),
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: 2,
        locals: vec![
            param_local("self", ref_variant_type, false),
            param_local("f", fmt_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

/// Build the body for variant `InspectAlt` (shared between generic and non-generic).
fn build_variant_inspect_alt_body(
    variant_name: &str,
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> Vec<TirStmt> {
    let deref_self = || {
        deref_expr(
            local_expr(0, "self", ref_variant_type, span),
            variant_type,
            span,
        )
    };
    let fmt_local = || local_expr(1, "f", fmt_type, span);

    let mut chain: Option<TirExpr> = None;
    for (case_name, case_index, payload_type) in cases.iter().rev() {
        let is_unit = *payload_type == TypeTable::UNIT;
        let mut then_stmts = Vec::new();

        if is_unit {
            then_stmts.push(write_str_stmt(
                format!("{variant_name}::{case_name}"),
                fmt_local(),
                string_type,
                span,
            ));
        } else {
            // f.open_brace("VariantName::CaseName(")
            then_stmts.push(formatter_call(
                "open_brace",
                fmt_local(),
                Some((format!("{variant_name}::{case_name}("), string_type)),
                span,
            ));
            // f.write_newline_indent()
            then_stmts.push(formatter_call(
                "write_newline_indent",
                fmt_local(),
                None::<(&str, TypeId)>,
                span,
            ));
            let payload = TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(deref_self()),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                *payload_type,
                span,
            );
            then_stmts.push(inspect_alt_call(
                payload,
                *payload_type,
                fmt_local(),
                module_source,
                tt,
                span,
            ));
            then_stmts.push(write_str_stmt(",", fmt_local(), string_type, span));
            // f.close_brace(")")
            then_stmts.push(formatter_call(
                "close_brace",
                fmt_local(),
                Some((")", string_type)),
                span,
            ));
        }

        let cond = TirExpr::new(
            TirExprKind::VariantTest {
                expr: Box::new(deref_self()),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TypeTable::BOOL,
            span,
        );
        let if_expr = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(cond),
                then_branch: TirBlock::new(then_stmts, span),
                else_branch: chain
                    .map(|e| TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(e), span)], span)),
            },
            TypeTable::UNIT,
            span,
        );
        chain = Some(if_expr);
    }

    chain.map_or_else(Vec::new, |e| vec![TirStmt::new(TirStmtKind::Expr(e), span)])
}

/// Build a `value.inspect_alt(f)` method call statement.
fn inspect_alt_call(
    value: TirExpr,
    value_type: TypeId,
    fmt: TirExpr,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirStmt {
    let call = trait_call_on_type(
        value,
        value_type,
        "InspectAlt",
        "inspect_alt",
        TypeTable::UNIT,
        vec![fmt],
        true,
        inspect_impl_module,
        module_source,
        tt,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Build a `f.method_name(text)` or `f.method_name()` call statement on the Formatter.
fn formatter_call(
    method_name: &str,
    fmt: TirExpr,
    text_arg: Option<(impl Into<String>, TypeId)>,
    span: Span,
) -> TirStmt {
    let args = match text_arg {
        Some((text, string_type)) => vec![CallArg::new(
            TirExpr::new(TirExprKind::StringLiteral(text.into()), string_type, span),
            false,
        )],
        None => vec![],
    };
    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(fmt),
            FunctionRef {
                module_source: ModuleSource::format(),
                name: format!("Formatter::{method_name}"),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    "Formatter".to_string(),
                    None,
                    method_name.to_string(),
                )),
            },
            vec![],
            args,
        ),
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

// ─── Display fallback synthesis ───

/// Generate `Display::fmt` fallback implementations for types without a user-provided Display impl.
///
/// The fallback simply delegates to `Inspect::inspect`:
/// ```text
/// fn fmt(&self, f: &mut Formatter) { self.inspect(f); }
/// ```
fn generate_display_fallback_impls(module: &mut TirModule) {
    let module_source = module.module_source.clone();
    let existing = collect_existing_trait_methods(module);
    let all_fn_names: IndexSet<String> = module
        .functions
        .iter()
        .filter_map(|f| f.try_borrow().ok().map(|func| func.name.clone()))
        .collect();
    let mut generated = Vec::new();

    let span = synth_span();
    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct("Formatter".to_string(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);

    // Helper: make Display/Inspect LocalMethodName pairs for a simple type
    let simple_pair = |name: &str| -> (LocalMethodName, LocalMethodName) {
        (
            LocalMethodName::new(
                name.to_string(),
                Some("Display".to_string()),
                "fmt".to_string(),
            ),
            LocalMethodName::new(
                name.to_string(),
                Some("Inspect".to_string()),
                "inspect".to_string(),
            ),
        )
    };

    // Enums
    for name in module
        .enums
        .iter()
        .map(|e| e.name.clone())
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "Display",
            "fmt",
            "Inspect",
            "inspect",
        ) {
            continue;
        }
        let enum_type = tt.make_enum(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(enum_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Non-generic structs
    for name in module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
    {
        if name == "String" || name == "Formatter" {
            continue;
        }
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "Display",
            "fmt",
            "Inspect",
            "inspect",
        ) {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(struct_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Generic structs
    for (name, type_params) in module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| (s.name.clone(), s.type_params.clone()))
        .collect::<Vec<_>>()
    {
        if name == "Array" {
            continue;
        }
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "Display",
            "fmt",
            "Inspect",
            "inspect",
        ) {
            continue;
        }
        let type_param_ids = make_type_param_ids(&type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(struct_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            type_params,
            span,
        ))));
    }

    // Non-generic variants
    for name in module
        .variants
        .iter()
        .filter(|v| v.type_params.is_empty())
        .map(|v| v.name.clone())
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "Display",
            "fmt",
            "Inspect",
            "inspect",
        ) {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(variant_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Generic variants (e.g., Option<T>, Result<T, E>)
    for (name, type_params) in module
        .variants
        .iter()
        .filter(|v| !v.type_params.is_empty())
        .map(|v| (v.name.clone(), v.type_params.clone()))
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "Display",
            "fmt",
            "Inspect",
            "inspect",
        ) {
            continue;
        }
        let type_param_ids = make_type_param_ids(&type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(variant_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            type_params,
            span,
        ))));
    }

    // Flags types
    for (name, flags_type_id) in module
        .flags
        .iter()
        .map(|f| (f.name.clone(), f.type_id))
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "Display",
            "fmt",
            "Inspect",
            "inspect",
        ) {
            continue;
        }
        let ref_type = tt.make_ref(flags_type_id);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Newtypes
    for nt in &module.newtypes {
        if module.flags.iter().any(|f| f.type_id == nt.type_id) {
            continue;
        }
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &nt.name,
            "Display",
            "fmt",
            "Inspect",
            "inspect",
        ) {
            continue;
        }
        let ref_type = tt.make_ref(nt.type_id);
        let (di, ii) = simple_pair(&nt.name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Parameterized types (function types, opaque types) — Display fallback
    // Tuples: Display is provided by variadic impl in core:prelude/tuple.wado
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        // TODO: collect_parameterized_types returns only base_name without module_source,
        // so we cannot use TypeTable::is_tuple_type here. This is safe because
        // collect_parameterized_types only returns TUPLE_TYPE_NAME for actual tuple types.
        if base_name == TypeTable::TUPLE_TYPE_NAME {
            continue;
        }
        let mangled = format_parameterized_name(&base_name, &type_arg_names);
        let display_key = format!("{mangled}^Display::fmt");
        if existing.contains(&display_key) {
            continue;
        }
        let inspect_key = format!("{mangled}^Inspect::inspect");
        if !existing.contains(&inspect_key) && !all_fn_names.contains(&inspect_key) {
            continue;
        }
        let ref_type = tt.make_ref(type_id);
        let di = LocalMethodName::new(
            base_name.clone(),
            Some("Display".to_string()),
            "fmt".to_string(),
        )
        .with_struct_type_args(&type_arg_names);
        let ii = LocalMethodName::new(
            base_name,
            Some("Inspect".to_string()),
            "inspect".to_string(),
        )
        .with_struct_type_args(&type_arg_names);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate a `Display::fmt` function that delegates to `self.inspect(f)`.
///
/// Used for all type categories (enums, structs, tuples, function types).
/// The `display_info` and `inspect_info` `LocalMethodName`s determine the exact mangled names.
/// `impl_type_params` is non-empty for generic structs.
fn generate_display_fallback(
    display_info: LocalMethodName,
    inspect_info: LocalMethodName,
    ref_type: TypeId,
    fmt_type: TypeId,
    module_source: &ModuleSource,
    impl_type_params: Vec<TirTypeParam>,
    span: Span,
) -> TirFunction {
    let qualified_name = display_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let self_local = TirExpr::new(
        TirExprKind::Local {
            index: 0,
            name: "self".to_string(),
        },
        ref_type,
        span,
    );
    let fmt_local = TirExpr::new(
        TirExprKind::Local {
            index: 1,
            name: "f".to_string(),
        },
        fmt_type,
        span,
    );

    let body = TirBlock::new(
        vec![trait_method_call(
            self_local,
            inspect_info,
            module_source.clone(),
            vec![fmt_local],
            span,
        )],
        span,
    );

    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(display_info),
        params,
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: 2,
        locals: vec![
            param_local("self", ref_type, false),
            param_local("f", fmt_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

// ─── DisplayAlt fallback synthesis ───

/// Generate `DisplayAlt::fmt_alt` fallback implementations that delegate to `InspectAlt::inspect_alt`.
///
/// Mirrors `generate_display_fallback_impls` but for the Alt trait pair.
fn generate_display_alt_fallback_impls(module: &mut TirModule) {
    let module_source = module.module_source.clone();
    let existing = collect_existing_trait_methods(module);
    let all_fn_names: IndexSet<String> = module
        .functions
        .iter()
        .filter_map(|f| f.try_borrow().ok().map(|func| func.name.clone()))
        .collect();
    let mut generated = Vec::new();

    let span = synth_span();
    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct("Formatter".to_string(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);

    let simple_pair = |name: &str| -> (LocalMethodName, LocalMethodName) {
        (
            LocalMethodName::new(
                name.to_string(),
                Some("DisplayAlt".to_string()),
                "fmt_alt".to_string(),
            ),
            LocalMethodName::new(
                name.to_string(),
                Some("InspectAlt".to_string()),
                "inspect_alt".to_string(),
            ),
        )
    };

    // Enums
    for name in module
        .enums
        .iter()
        .map(|e| e.name.clone())
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "DisplayAlt",
            "fmt_alt",
            "InspectAlt",
            "inspect_alt",
        ) {
            continue;
        }
        let enum_type = tt.make_enum(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(enum_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Non-generic structs
    for name in module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| s.name.clone())
        .collect::<Vec<_>>()
    {
        if name == "String" || name == "Formatter" {
            continue;
        }
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "DisplayAlt",
            "fmt_alt",
            "InspectAlt",
            "inspect_alt",
        ) {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(struct_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Generic structs
    for (name, type_params) in module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| (s.name.clone(), s.type_params.clone()))
        .collect::<Vec<_>>()
    {
        if name == "Array" {
            continue;
        }
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "DisplayAlt",
            "fmt_alt",
            "InspectAlt",
            "inspect_alt",
        ) {
            continue;
        }
        let type_param_ids = make_type_param_ids(&type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(struct_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            type_params,
            span,
        ))));
    }

    // Non-generic variants
    for name in module
        .variants
        .iter()
        .filter(|v| v.type_params.is_empty())
        .map(|v| v.name.clone())
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "DisplayAlt",
            "fmt_alt",
            "InspectAlt",
            "inspect_alt",
        ) {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(variant_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Generic variants
    for (name, type_params) in module
        .variants
        .iter()
        .filter(|v| !v.type_params.is_empty())
        .map(|v| (v.name.clone(), v.type_params.clone()))
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "DisplayAlt",
            "fmt_alt",
            "InspectAlt",
            "inspect_alt",
        ) {
            continue;
        }
        let type_param_ids = make_type_param_ids(&type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(variant_type);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            type_params,
            span,
        ))));
    }

    // Flags
    for (name, flags_type_id) in module
        .flags
        .iter()
        .map(|f| (f.name.clone(), f.type_id))
        .collect::<Vec<_>>()
    {
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &name,
            "DisplayAlt",
            "fmt_alt",
            "InspectAlt",
            "inspect_alt",
        ) {
            continue;
        }
        let ref_type = tt.make_ref(flags_type_id);
        let (di, ii) = simple_pair(&name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Newtypes
    for nt in &module.newtypes {
        if module.flags.iter().any(|f| f.type_id == nt.type_id) {
            continue;
        }
        if !should_generate_fallback(
            &existing,
            &all_fn_names,
            &nt.name,
            "DisplayAlt",
            "fmt_alt",
            "InspectAlt",
            "inspect_alt",
        ) {
            continue;
        }
        let ref_type = tt.make_ref(nt.type_id);
        let (di, ii) = simple_pair(&nt.name);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    // Parameterized types
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        let mangled = format_parameterized_name(&base_name, &type_arg_names);
        let da_key = format!("{mangled}^DisplayAlt::fmt_alt");
        if existing.contains(&da_key) {
            continue;
        }
        let ia_key = format!("{mangled}^InspectAlt::inspect_alt");
        if !existing.contains(&ia_key) && !all_fn_names.contains(&ia_key) {
            continue;
        }
        let ref_type = tt.make_ref(type_id);
        let di = LocalMethodName::new(
            base_name.clone(),
            Some("DisplayAlt".to_string()),
            "fmt_alt".to_string(),
        )
        .with_struct_type_args(&type_arg_names);
        let ii = LocalMethodName::new(
            base_name,
            Some("InspectAlt".to_string()),
            "inspect_alt".to_string(),
        )
        .with_struct_type_args(&type_arg_names);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            di,
            ii,
            ref_type,
            fmt_type,
            &module_source,
            vec![],
            span,
        ))));
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Build a `value.inspect(f)` method call statement.
fn inspect_call(
    value: TirExpr,
    value_type: TypeId,
    fmt: TirExpr,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirStmt {
    let call = trait_call_on_type(
        value,
        value_type,
        "Inspect",
        "inspect",
        TypeTable::UNIT,
        vec![fmt],
        true,
        inspect_impl_module,
        module_source,
        tt,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Decompose a type into `(base_name, is_type_param, type_arg_names)` for `LocalMethodName`.
///
/// All parameterized types are explicitly handled to ensure the base name never
/// contains `<`, which would cause `LocalMethodName::new` to panic.
fn decompose_type_for_method_name(
    resolved: &ResolvedType,
    type_id: TypeId,
    tt: &TypeTable,
) -> (String, bool, Vec<String>) {
    match resolved {
        ResolvedType::TypeParam { name, .. } => (name.clone(), true, vec![]),
        ResolvedType::BuiltinArray(elem) => (
            "builtin::array".to_string(),
            false,
            vec![tt.mangle_type_name(*elem)],
        ),
        ResolvedType::GenericInstance {
            name, type_args, ..
        } => {
            let args = type_args.iter().map(|t| tt.mangle_type_name(*t)).collect();
            (name.clone(), false, args)
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            let args = vec![params.len().to_string(), tt.mangle_type_name(*return_type)];
            ("Fn".to_string(), false, args)
        }
        ResolvedType::GenericResource {
            name, type_args, ..
        } => {
            let args = type_args.iter().map(|t| tt.mangle_type_name(*t)).collect();
            (name.clone(), false, args)
        }
        ResolvedType::Reactive(inner) => (
            "Reactive".to_string(),
            false,
            vec![tt.mangle_type_name(*inner)],
        ),
        ResolvedType::Ref(inner) => ("&".to_string(), false, vec![tt.mangle_type_name(*inner)]),
        ResolvedType::MutRef(inner) => {
            ("&mut".to_string(), false, vec![tt.mangle_type_name(*inner)])
        }
        _ => {
            let name = tt.mangle_type_name(type_id);
            debug_assert!(
                !name.contains('<'),
                "decompose_type_for_method_name: unhandled parameterized type: {name}"
            );
            (name, false, vec![])
        }
    }
}

/// Determine the module where an Inspect impl lives for a given type.
/// Determine the module where a trait impl lives for a given type.
///
/// `ref_module` is used for Ref/MutRef types (`traits()` for Eq/Ord, `format()` for Inspect).
/// `string_module` is used for String (`string()` for Eq/Ord, `format()` for Inspect).
fn trait_impl_module(
    type_id: TypeId,
    tt: &TypeTable,
    default: &ModuleSource,
    ref_module: ModuleSource,
    string_module: ModuleSource,
) -> ModuleSource {
    match tt.get(type_id).clone() {
        ResolvedType::Primitive(_) => ModuleSource::primitive(),
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => ref_module,
        ResolvedType::Struct { ref name, .. } if name == "String" => string_module,
        ResolvedType::Struct {
            ref module_source, ..
        }
        | ResolvedType::Enum {
            ref module_source, ..
        }
        | ResolvedType::Variant {
            ref module_source, ..
        }
        | ResolvedType::GenericInstance {
            ref module_source, ..
        } => module_source.clone(),
        _ => default.clone(),
    }
}

fn inspect_impl_module(type_id: TypeId, tt: &TypeTable, default: &ModuleSource) -> ModuleSource {
    trait_impl_module(
        type_id,
        tt,
        default,
        ModuleSource::format(),
        ModuleSource::format(),
    )
}

/// Collect parameterized types that need Inspect/Display impls.
///
/// Returns `(type_id, base_name, type_arg_names)` for each concrete parameterized type.
/// Includes tuples, function types, and resource handle types (Future, Stream, etc.).
fn collect_parameterized_types(tt: &TypeTable) -> Vec<(TypeId, String, Vec<String>)> {
    let is_concrete = |t: TypeId| !matches!(tt.get(t), ResolvedType::TypeParam { .. });

    tt.all_types()
        .filter_map(|(id, resolved)| match resolved {
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source,
            } if TypeTable::is_tuple_type(name, module_source) => {
                if !type_args.iter().all(|e| is_concrete(*e)) {
                    return None;
                }
                let args = type_args.iter().map(|e| tt.mangle_type_name(*e)).collect();
                Some((*id, TypeTable::TUPLE_TYPE_NAME.to_string(), args))
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                if !params.iter().all(|p| is_concrete(*p)) || !is_concrete(*return_type) {
                    return None;
                }
                let args = vec![params.len().to_string(), tt.mangle_type_name(*return_type)];
                Some((*id, "Fn".to_string(), args))
            }
            ResolvedType::GenericResource {
                name, type_args, ..
            } => {
                if !type_args.iter().all(|t| is_concrete(*t)) {
                    return None;
                }
                let args = type_args.iter().map(|t| tt.mangle_type_name(*t)).collect();
                Some((*id, name.clone(), args))
            }
            _ => None,
        })
        .collect()
}

/// Format a parameterized type's mangled name from base name and type arg names.
fn format_parameterized_name(base_name: &str, type_arg_names: &[String]) -> String {
    if type_arg_names.is_empty() {
        base_name.to_string()
    } else {
        format!("{}<{}>", base_name, type_arg_names.join(","))
    }
}

// ─── Eq/Ord generators (existing) ───

/// Generate `EnumName^Eq::eq(&self, &Self) -> bool`
///
/// Body: `return *self == *other;` (i32 comparison via enum discriminant)
fn generate_enum_eq_fn(
    enum_name: &str,
    enum_type: TypeId,
    ref_enum_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        enum_name.to_string(),
        Some("Eq".to_string()),
        "eq".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_enum_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_enum_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let deref_self = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 0,
                    name: "self".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );
    let deref_other = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 1,
                    name: "other".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );
    let comparison = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(deref_self),
            op: TirBinaryOp::Eq,
            right: Box::new(deref_other),
        },
        TypeTable::BOOL,
        span,
    );
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(comparison),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        TypeTable::BOOL,
        body,
        vec![
            param_local("self", ref_enum_type, false),
            param_local("other", ref_enum_type, false),
        ],
    )
}

/// Generate `EnumName^Ord::cmp(&self, &Self) -> Ordering`
///
/// Body:
/// ```text
/// let a = *self;
/// let b = *other;
/// if a < b { return Ordering::Less; }
/// if a > b { return Ordering::Greater; }
/// return Ordering::Equal;
/// ```
fn generate_enum_ord_fn(
    enum_name: &str,
    enum_type: TypeId,
    ref_enum_type: TypeId,
    ordering_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        enum_name.to_string(),
        Some("Ord".to_string()),
        "cmp".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_enum_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_enum_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let deref_self = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 0,
                    name: "self".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );
    let deref_other = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(TirExpr::new(
                TirExprKind::Local {
                    index: 1,
                    name: "other".to_string(),
                },
                ref_enum_type,
                span,
            )),
        },
        enum_type,
        span,
    );

    let local_a = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 2,
                name: "a".to_string(),
            },
            enum_type,
            span,
        )
    };
    let local_b = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 3,
                name: "b".to_string(),
            },
            enum_type,
            span,
        )
    };

    let ordering_less = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 0,
            case_name: "Less".to_string(),
        },
        ordering_type,
        span,
    );
    let ordering_greater = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 2,
            case_name: "Greater".to_string(),
        },
        ordering_type,
        span,
    );
    let ordering_equal = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 1,
            case_name: "Equal".to_string(),
        },
        ordering_type,
        span,
    );

    let cond_lt = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(local_a(span)),
            op: TirBinaryOp::Lt,
            right: Box::new(local_b(span)),
        },
        TypeTable::BOOL,
        span,
    );
    let if_lt = TirStmt::new(
        TirStmtKind::If {
            condition: cond_lt,
            then_block: TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Return {
                        value: Some(ordering_less),
                    },
                    span,
                )],
                span,
            ),
            else_block: None,
        },
        span,
    );

    let cond_gt = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(local_a(span)),
            op: TirBinaryOp::Gt,
            right: Box::new(local_b(span)),
        },
        TypeTable::BOOL,
        span,
    );
    let if_gt = TirStmt::new(
        TirStmtKind::If {
            condition: cond_gt,
            then_block: TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Return {
                        value: Some(ordering_greater),
                    },
                    span,
                )],
                span,
            ),
            else_block: None,
        },
        span,
    );

    let return_equal = TirStmt::new(
        TirStmtKind::Return {
            value: Some(ordering_equal),
        },
        span,
    );

    let body = TirBlock::new(
        vec![
            TirStmt::new(
                TirStmtKind::Let {
                    name: "a".to_string(),
                    local_index: 2,
                    is_mut: false,
                    is_reactive: false,
                    type_id: enum_type,
                    value: deref_self,
                    skip_value_copy: false,
                },
                span,
            ),
            TirStmt::new(
                TirStmtKind::Let {
                    name: "b".to_string(),
                    local_index: 3,
                    is_mut: false,
                    is_reactive: false,
                    type_id: enum_type,
                    value: deref_other,
                    skip_value_copy: false,
                },
                span,
            ),
            if_lt,
            if_gt,
            return_equal,
        ],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        ordering_type,
        body,
        vec![
            param_local("self", ref_enum_type, false),
            param_local("other", ref_enum_type, false),
            TirLocal::synth(2, enum_type, false),
            TirLocal::synth(3, enum_type, false),
        ],
    )
}

/// Build a trait method call on a value: `value.Trait::method(args...)`.
///
/// Handles type decomposition, method name mangling, impl module resolution,
/// and Ref/MutRef monomorphization. The value is automatically wrapped in a reference.
fn trait_call_on_type(
    value: TirExpr,
    value_type: TypeId,
    trait_name: &str,
    method_name: &str,
    return_type: TypeId,
    args: Vec<TirExpr>,
    needs_ref_monomorph: bool,
    resolve_impl_module: fn(TypeId, &TypeTable, &ModuleSource) -> ModuleSource,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirExpr {
    let ref_type = tt.make_ref(value_type);
    let receiver = ref_expr(value, ref_type, span);

    let resolved = tt.get(value_type).clone();
    let (base_name, is_type_param, type_arg_names) =
        decompose_type_for_method_name(&resolved, value_type, tt);

    let mut info = LocalMethodName::new(
        base_name,
        Some(trait_name.to_string()),
        method_name.to_string(),
    );
    if !type_arg_names.is_empty() {
        info = info.with_struct_type_args(&type_arg_names);
    }
    info.is_type_param_receiver = is_type_param;

    let impl_module = if is_type_param {
        module_source.clone()
    } else {
        resolve_impl_module(value_type, tt, module_source)
    };

    let monomorph_info = if needs_ref_monomorph {
        match &resolved {
            ResolvedType::Ref(inner_id) | ResolvedType::MutRef(inner_id) => {
                let base_info = LocalMethodName::new(
                    info.base_struct_name.clone(),
                    Some(trait_name.to_string()),
                    method_name.to_string(),
                );
                Some(MonomorphInfo {
                    generic_name: base_info.to_mangled_name(),
                    impl_type_args: vec![*inner_id],
                    method_type_args: vec![],
                    is_blanket: true,
                })
            }
            _ => None,
        }
    } else {
        None
    };

    let fn_name = info.to_mangled_name();
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: impl_module,
                name: fn_name,
                monomorph_info,
                method_info: Some(info),
            },
            vec![],
            args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        ),
        return_type,
        span,
    )
}

/// Build an `Eq::eq` method call on a field value: `self.field.eq(&other.field)`.
fn eq_call_expr(
    self_field: TirExpr,
    other_field: TirExpr,
    field_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirExpr {
    let ref_type = tt.make_ref(field_type);
    let arg = ref_expr(other_field, ref_type, span);
    trait_call_on_type(
        self_field,
        field_type,
        "Eq",
        "eq",
        TypeTable::BOOL,
        vec![arg],
        true,
        eq_impl_module,
        module_source,
        tt,
        span,
    )
}

/// Build an `Ord::cmp` method call on a field value: `self.field.cmp(&other.field)`.
fn cmp_call_expr(
    self_field: TirExpr,
    other_field: TirExpr,
    field_type: TypeId,
    ordering_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirExpr {
    let ref_type = tt.make_ref(field_type);
    let arg = ref_expr(other_field, ref_type, span);
    trait_call_on_type(
        self_field,
        field_type,
        "Ord",
        "cmp",
        ordering_type,
        vec![arg],
        false,
        eq_impl_module,
        module_source,
        tt,
        span,
    )
}

/// Resolve the module source for a type's Eq/Ord implementation.
fn eq_impl_module(type_id: TypeId, tt: &TypeTable, default: &ModuleSource) -> ModuleSource {
    trait_impl_module(
        type_id,
        tt,
        default,
        ModuleSource::traits(),
        ModuleSource::string(),
    )
}

/// Generate `StructName^Eq::eq(&self, &Self) -> bool` for non-generic structs.
///
/// Body: `return self.f0 == other.f0 && self.f1 == other.f1 && ...`
/// Empty structs: `return true;`
fn generate_struct_eq_fn(
    struct_name: &str,
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("Eq".to_string()),
        "eq".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_struct_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_struct_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let self_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 0,
                name: "self".to_string(),
            },
            ref_struct_type,
            span,
        )
    };
    let other_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "other".to_string(),
            },
            ref_struct_type,
            span,
        )
    };

    let result = if fields.is_empty() {
        // Empty struct: always equal
        TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span)
    } else {
        // Build: self.f0.eq(&other.f0) && self.f1.eq(&other.f1) && ...
        let mut iter = fields.iter();
        let (first_name, first_type, first_index) = iter.next().unwrap();
        let self_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(self_ref(span)),
                field_index: *first_index,
                field_name: first_name.clone(),
            },
            *first_type,
            span,
        );
        let other_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(other_ref(span)),
                field_index: *first_index,
                field_name: first_name.clone(),
            },
            *first_type,
            span,
        );
        let mut result = eq_call_expr(
            self_field,
            other_field,
            *first_type,
            module_source,
            tt,
            span,
        );

        for (field_name, field_type, field_index) in iter {
            let self_field = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(self_ref(span)),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            let other_field = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(other_ref(span)),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            let cmp = eq_call_expr(
                self_field,
                other_field,
                *field_type,
                module_source,
                tt,
                span,
            );
            result = TirExpr::new(
                TirExprKind::Binary {
                    op: TirBinaryOp::And,
                    left: Box::new(result),
                    right: Box::new(cmp),
                },
                TypeTable::BOOL,
                span,
            );
        }
        result
    };

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(result),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        TypeTable::BOOL,
        body,
        vec![
            param_local("self", ref_struct_type, false),
            param_local("other", ref_struct_type, false),
        ],
    )
}

/// Generate `StructName^Ord::cmp(&self, &Self) -> Ordering` for non-generic structs.
///
/// Body (lexicographic):
/// ```text
/// let c = self.f0.cmp(&other.f0);
/// if c != Ordering::Equal { return c; }
/// let c = self.f1.cmp(&other.f1);
/// if c != Ordering::Equal { return c; }
/// ...
/// return Ordering::Equal;
/// ```
fn generate_struct_ord_fn(
    struct_name: &str,
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    ordering_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("Ord".to_string()),
        "cmp".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_struct_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_struct_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let self_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 0,
                name: "self".to_string(),
            },
            ref_struct_type,
            span,
        )
    };
    let other_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "other".to_string(),
            },
            ref_struct_type,
            span,
        )
    };

    let ordering_equal = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 1,
            case_name: "Equal".to_string(),
        },
        ordering_type,
        span,
    );

    let mut stmts = Vec::new();
    let mut locals = vec![
        param_local("self", ref_struct_type, false),
        param_local("other", ref_struct_type, false),
    ];

    // Local indices start at 2 (0 = self, 1 = other).
    for (local_idx, (field_name, field_type, field_index)) in (2_u32..).zip(fields.iter()) {
        let self_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(self_ref(span)),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            *field_type,
            span,
        );
        let other_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(other_ref(span)),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            *field_type,
            span,
        );
        let cmp_result = cmp_call_expr(
            self_field,
            other_field,
            *field_type,
            ordering_type,
            module_source,
            tt,
            span,
        );

        locals.push(TirLocal {
            name: "c".to_string(),
            type_id: ordering_type,
            is_mut: false,
        });

        // let c = self.field.cmp(&other.field);
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: "c".to_string(),
                local_index: local_idx,
                is_mut: false,
                is_reactive: false,
                type_id: ordering_type,
                value: cmp_result,
                skip_value_copy: false,
            },
            span,
        ));

        // if c != Ordering::Equal { return c; }
        let local_c = TirExpr::new(
            TirExprKind::Local {
                index: local_idx,
                name: "c".to_string(),
            },
            ordering_type,
            span,
        );
        let cond = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::NotEq,
                left: Box::new(local_c.clone()),
                right: Box::new(ordering_equal.clone()),
            },
            TypeTable::BOOL,
            span,
        );
        stmts.push(TirStmt::new(
            TirStmtKind::If {
                condition: cond,
                then_block: TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(local_c),
                        },
                        span,
                    )],
                    span,
                ),
                else_block: None,
            },
            span,
        ));
    }

    // return Ordering::Equal;
    stmts.push(TirStmt::new(
        TirStmtKind::Return {
            value: Some(ordering_equal),
        },
        span,
    ));

    let body = TirBlock::new(stmts, span);

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        ordering_type,
        body,
        locals,
    )
}

/// Generate `StructName^Eq::eq(&self, &Self) -> bool` for generic structs.
///
/// Sets `impl_type_params` with Eq bounds so the monomorphizer can
/// specialize field eq calls for concrete types.
fn generate_generic_struct_eq_fn(
    struct_name: &str,
    type_params: &[TirTypeParam],
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("Eq".to_string()),
        "eq".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_struct_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_struct_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let self_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 0,
                name: "self".to_string(),
            },
            ref_struct_type,
            span,
        )
    };
    let other_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "other".to_string(),
            },
            ref_struct_type,
            span,
        )
    };

    let result = if fields.is_empty() {
        TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span)
    } else {
        let mut iter = fields.iter();
        let (first_name, first_type, first_index) = iter.next().unwrap();
        let self_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(self_ref(span)),
                field_index: *first_index,
                field_name: first_name.clone(),
            },
            *first_type,
            span,
        );
        let other_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(other_ref(span)),
                field_index: *first_index,
                field_name: first_name.clone(),
            },
            *first_type,
            span,
        );
        let mut result = eq_call_expr(
            self_field,
            other_field,
            *first_type,
            module_source,
            tt,
            span,
        );

        for (field_name, field_type, field_index) in iter {
            let self_field = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(self_ref(span)),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            let other_field = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(other_ref(span)),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                *field_type,
                span,
            );
            let cmp = eq_call_expr(
                self_field,
                other_field,
                *field_type,
                module_source,
                tt,
                span,
            );
            result = TirExpr::new(
                TirExprKind::Binary {
                    op: TirBinaryOp::And,
                    left: Box::new(result),
                    right: Box::new(cmp),
                },
                TypeTable::BOOL,
                span,
            );
        }
        result
    };

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(result),
            },
            span,
        )],
        span,
    );

    let impl_type_params: Vec<TirTypeParam> = type_params.to_vec();

    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(method_info),
        params,
        return_type: TypeTable::BOOL,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: 2,
        locals: vec![
            param_local("self", ref_struct_type, false),
            param_local("other", ref_struct_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

/// Generate `VariantName^Eq::eq(&self, &Self) -> bool` for non-generic variants.
///
/// Body: if-else chain testing each case with `VariantTest`, comparing payloads via `eq_call_expr`.
fn generate_variant_eq_fn(
    variant_name: &str,
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        variant_name.to_string(),
        Some("Eq".to_string()),
        "eq".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_variant_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_variant_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let deref_self = || {
        deref_expr(
            TirExpr::new(
                TirExprKind::Local {
                    index: 0,
                    name: "self".to_string(),
                },
                ref_variant_type,
                span,
            ),
            variant_type,
            span,
        )
    };
    let deref_other = || {
        deref_expr(
            TirExpr::new(
                TirExprKind::Local {
                    index: 1,
                    name: "other".to_string(),
                },
                ref_variant_type,
                span,
            ),
            variant_type,
            span,
        )
    };

    let body_stmts = variant_eq_body(cases, &deref_self, &deref_other, module_source, tt, span);
    let body = TirBlock::new(body_stmts, span);

    make_synthetic_method(
        qualified_name,
        method_info,
        params,
        TypeTable::BOOL,
        body,
        vec![
            param_local("self", ref_variant_type, false),
            param_local("other", ref_variant_type, false),
        ],
    )
}

/// Generate `VariantName^Eq::eq(&self, &Self) -> bool` for generic variants.
///
/// Same structure as `generate_variant_eq_fn` but with `impl_type_params` set so the
/// monomorphizer can specialize payload comparisons for each concrete instantiation.
fn generate_generic_variant_eq_fn(
    variant_name: &str,
    type_params: &[TirTypeParam],
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        variant_name.to_string(),
        Some("Eq".to_string()),
        "eq".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_variant_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_variant_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let deref_self = || {
        deref_expr(
            TirExpr::new(
                TirExprKind::Local {
                    index: 0,
                    name: "self".to_string(),
                },
                ref_variant_type,
                span,
            ),
            variant_type,
            span,
        )
    };
    let deref_other = || {
        deref_expr(
            TirExpr::new(
                TirExprKind::Local {
                    index: 1,
                    name: "other".to_string(),
                },
                ref_variant_type,
                span,
            ),
            variant_type,
            span,
        )
    };

    let body_stmts = variant_eq_body(cases, &deref_self, &deref_other, module_source, tt, span);
    let body = TirBlock::new(body_stmts, span);

    let impl_type_params: Vec<TirTypeParam> = type_params.to_vec();

    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(method_info),
        params,
        return_type: TypeTable::BOOL,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: 2,
        locals: vec![
            param_local("self", ref_variant_type, false),
            param_local("other", ref_variant_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}

/// Build the body statements for variant Eq: a chain of if-else testing each case.
///
/// For each case:
/// - If both `self` and `other` are that case, compare payloads (or return true for unit cases)
/// - Otherwise fall through to the next case
/// - Final fallback: return false (different cases)
fn variant_eq_body(
    cases: &[(String, u32, TypeId)],
    deref_self: &dyn Fn() -> TirExpr,
    deref_other: &dyn Fn() -> TirExpr,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> Vec<TirStmt> {
    let mut stmts = Vec::new();

    for (case_name, case_index, payload_type) in cases {
        let is_unit = *payload_type == TypeTable::UNIT;

        // Condition: self is this case
        let self_test = TirExpr::new(
            TirExprKind::VariantTest {
                expr: Box::new(deref_self()),
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            TypeTable::BOOL,
            span,
        );

        let then_stmts = if is_unit {
            // Unit case: return whether other is the same case
            let other_test = TirExpr::new(
                TirExprKind::VariantTest {
                    expr: Box::new(deref_other()),
                    case_index: *case_index,
                    case_name: case_name.clone(),
                },
                TypeTable::BOOL,
                span,
            );
            vec![TirStmt::new(
                TirStmtKind::Return {
                    value: Some(other_test),
                },
                span,
            )]
        } else {
            // Payload case: if other is the same case, compare payloads; else return false
            let other_test = TirExpr::new(
                TirExprKind::VariantTest {
                    expr: Box::new(deref_other()),
                    case_index: *case_index,
                    case_name: case_name.clone(),
                },
                TypeTable::BOOL,
                span,
            );

            let self_payload = TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(deref_self()),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                *payload_type,
                span,
            );
            let other_payload = TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(deref_other()),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                *payload_type,
                span,
            );

            let eq_result = eq_call_expr(
                self_payload,
                other_payload,
                *payload_type,
                module_source,
                tt,
                span,
            );

            let inner_if = TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(other_test),
                    then_branch: TirBlock::new(
                        vec![TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(eq_result),
                            },
                            span,
                        )],
                        span,
                    ),
                    else_branch: Some(TirBlock::new(
                        vec![TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(TirExpr::new(
                                    TirExprKind::BoolLiteral(false),
                                    TypeTable::BOOL,
                                    span,
                                )),
                            },
                            span,
                        )],
                        span,
                    )),
                },
                TypeTable::UNIT,
                span,
            );

            vec![TirStmt::new(TirStmtKind::Expr(inner_if), span)]
        };

        let if_expr = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(self_test),
                then_branch: TirBlock::new(then_stmts, span),
                else_branch: None,
            },
            TypeTable::UNIT,
            span,
        );
        stmts.push(TirStmt::new(TirStmtKind::Expr(if_expr), span));
    }

    // Final fallback: return false (unreachable if all cases are covered)
    stmts.push(TirStmt::new(
        TirStmtKind::Return {
            value: Some(TirExpr::new(
                TirExprKind::BoolLiteral(false),
                TypeTable::BOOL,
                span,
            )),
        },
        span,
    ));

    stmts
}

/// Generate `StructName^Ord::cmp(&self, &Self) -> Ordering` for generic structs.
///
/// Sets `impl_type_params` with Ord bounds so the monomorphizer can
/// specialize field cmp calls for concrete types.
fn generate_generic_struct_ord_fn(
    struct_name: &str,
    type_params: &[TirTypeParam],
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    ordering_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        struct_name.to_string(),
        Some("Ord".to_string()),
        "cmp".to_string(),
    );
    let qualified_name = method_info.to_mangled_name();

    let params = vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_struct_type,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_struct_type,
            local_index: 1,
            is_mut: false,
            span,
            default_expr: None,
        },
    ];

    let self_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 0,
                name: "self".to_string(),
            },
            ref_struct_type,
            span,
        )
    };
    let other_ref = |span: Span| {
        TirExpr::new(
            TirExprKind::Local {
                index: 1,
                name: "other".to_string(),
            },
            ref_struct_type,
            span,
        )
    };

    let ordering_equal = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index: 1,
            case_name: "Equal".to_string(),
        },
        ordering_type,
        span,
    );

    let mut stmts = Vec::new();
    let mut locals = vec![
        param_local("self", ref_struct_type, false),
        param_local("other", ref_struct_type, false),
    ];

    // Local indices start at 2 (0 = self, 1 = other).
    for (local_idx, (field_name, field_type, field_index)) in (2_u32..).zip(fields.iter()) {
        let self_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(self_ref(span)),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            *field_type,
            span,
        );
        let other_field = TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(other_ref(span)),
                field_index: *field_index,
                field_name: field_name.clone(),
            },
            *field_type,
            span,
        );
        let cmp_result = cmp_call_expr(
            self_field,
            other_field,
            *field_type,
            ordering_type,
            module_source,
            tt,
            span,
        );

        locals.push(TirLocal {
            name: "c".to_string(),
            type_id: ordering_type,
            is_mut: false,
        });

        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: "c".to_string(),
                local_index: local_idx,
                is_mut: false,
                is_reactive: false,
                type_id: ordering_type,
                value: cmp_result,
                skip_value_copy: false,
            },
            span,
        ));

        let local_c = TirExpr::new(
            TirExprKind::Local {
                index: local_idx,
                name: "c".to_string(),
            },
            ordering_type,
            span,
        );
        let cond = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::NotEq,
                left: Box::new(local_c.clone()),
                right: Box::new(ordering_equal.clone()),
            },
            TypeTable::BOOL,
            span,
        );
        stmts.push(TirStmt::new(
            TirStmtKind::If {
                condition: cond,
                then_block: TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(local_c),
                        },
                        span,
                    )],
                    span,
                ),
                else_block: None,
            },
            span,
        ));
    }

    stmts.push(TirStmt::new(
        TirStmtKind::Return {
            value: Some(ordering_equal),
        },
        span,
    ));

    let body = TirBlock::new(stmts, span);

    let impl_type_params: Vec<TirTypeParam> = type_params.to_vec();

    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(method_info),
        params,
        return_type: ordering_type,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: locals.len() as u32,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    }
}
