//! Trait synthesis phase.
//!
//! Generates auto-derived trait implementations for types that support them:
//! - `EnumName^Eq::eq(&self, &Self) -> bool` - discriminant equality
//! - `EnumName^Ord::cmp(&self, &Self) -> Ordering` - discriminant ordering
//! - `TypeName^Inspect::inspect(&self, &mut Formatter)` - debug formatting
//! - `TypeName^Display::fmt(&self, &mut Formatter)` - display fallback (delegates to Inspect)
//!
//! Pipeline position: runs as part of the synthesis phase, before monomorphize.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexSet;

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::project::Project;
use crate::tir::{
    InlineHint, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule,
    TirParam, TirStmt, TirStmtKind, TirTypeParam, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::common::{
    deref_expr, field_access, make_synthetic_method, ref_expr, synth_span, trait_method_call,
    write_str_stmt,
};

/// Run trait synthesis on the entire project.
///
/// For each module, generates Eq/Ord, Inspect, and Display fallback implementations
/// for types that don't already have user-provided implementations.
pub fn synthesize_traits(project: Project) -> Project {
    let mut project = project;
    for module in project.tir_modules.values_mut() {
        generate_enum_trait_impls(module);
        generate_inspect_impls(module);
        generate_display_fallback_impls(module);
    }
    project
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

        let eq_key = MethodName::format_local(enum_name, Some("Eq"), "eq");
        if !existing_trait_methods.contains(&eq_key) {
            let func = generate_enum_eq_fn(enum_name, enum_type, ref_enum_type, *span);
            generated_functions.push(Rc::new(RefCell::new(func)));
        }

        let cmp_key = MethodName::format_local(enum_name, Some("Ord"), "cmp");
        if !existing_trait_methods.contains(&cmp_key) {
            let ordering_type =
                type_table.make_enum("Ordering".to_string(), ModuleSource::traits());
            let func =
                generate_enum_ord_fn(enum_name, enum_type, ref_enum_type, ordering_type, *span);
            generated_functions.push(Rc::new(RefCell::new(func)));
        }
    }

    module.functions.extend(generated_functions);
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
        let key = MethodName::format_local(name, Some("Inspect"), "inspect");
        if existing.contains(&key) {
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
    let struct_infos: Vec<_> = module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields: Vec<_> = s
                .fields
                .iter()
                .filter(|f| !f.is_hidden)
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            let has_hidden = s.fields.iter().any(|f| f.is_hidden);
            (s.name.clone(), fields, has_hidden, s.span)
        })
        .collect();

    for (name, fields, has_hidden, sspan) in &struct_infos {
        if name == "String" || name == "Formatter" {
            continue;
        }
        let key = MethodName::format_local(name, Some("Inspect"), "inspect");
        if existing.contains(&key) {
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
    let generic_struct_infos: Vec<_> = module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields: Vec<_> = s
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
        .collect();

    for (name, type_params, fields, has_hidden, sspan) in &generic_struct_infos {
        let key = MethodName::format_local(name, Some("Inspect"), "inspect");
        if existing.contains(&key) {
            continue;
        }
        let type_param_ids: Vec<TypeId> = type_params
            .iter()
            .map(|tp| tt.make_type_param(tp.name.clone(), tp.index))
            .collect();
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
    let variant_infos: Vec<_> = module
        .variants
        .iter()
        .filter(|v| v.type_params.is_empty())
        .map(|v| {
            let cases: Vec<_> = v
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.payload))
                .collect();
            (v.name.clone(), cases, v.span)
        })
        .collect();

    for (name, cases, vspan) in &variant_infos {
        let key = MethodName::format_local(name, Some("Inspect"), "inspect");
        if existing.contains(&key) {
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
    let generic_variant_infos: Vec<_> = module
        .variants
        .iter()
        .filter(|v| !v.type_params.is_empty())
        .map(|v| {
            let cases: Vec<_> = v
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.payload))
                .collect();
            (v.name.clone(), v.type_params.clone(), cases, v.span)
        })
        .collect();

    for (name, type_params, cases, vspan) in &generic_variant_infos {
        let key = MethodName::format_local(name, Some("Inspect"), "inspect");
        if existing.contains(&key) {
            continue;
        }
        let type_param_ids: Vec<TypeId> = type_params
            .iter()
            .map(|tp| tt.make_type_param(tp.name.clone(), tp.index))
            .collect();
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
        let key = MethodName::format_local(name, Some("Inspect"), "inspect");
        if existing.contains(&key) {
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
        let key = MethodName::format_local(&nt.name, Some("Inspect"), "inspect");
        if existing.contains(&key) {
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
            ResolvedType::Tuple(elements) => {
                generated.push(Rc::new(RefCell::new(generate_tuple_inspect_fn(
                    &type_arg_names,
                    &elements,
                    type_id,
                    ref_type,
                    fmt_type,
                    string_type,
                    &module_source,
                    &mut tt,
                    span,
                ))));
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                generated.push(Rc::new(RefCell::new(generate_fn_inspect_fn(
                    &type_arg_names,
                    &params,
                    return_type,
                    type_id,
                    ref_type,
                    fmt_type,
                    string_type,
                    &mut tt,
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
            span,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            span,
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
        vec![ref_enum_type, fmt_type],
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
            span,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            span,
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
        vec![ref_struct_type, fmt_type],
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
            span,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            span,
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
        effects: Vec::new(),
        body: Some(body),
        span,
        local_count,
        local_types: vec![ref_struct_type, fmt_type],
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
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
            span,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            span,
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
        vec![ref_variant_type, fmt_type],
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
            span,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            span,
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
        effects: Vec::new(),
        body: Some(body),
        span,
        local_count: 2,
        local_types: vec![ref_variant_type, fmt_type],
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
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
        vec![ref_newtype_type, fmt_type],
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
        vec![ref_flags_type, fmt_type],
    )
}

/// Generate `Tuple<A,B,...>^Inspect::inspect(&self, &mut Formatter)` for a concrete tuple type.
///
/// Body: writes `[elem0, elem1, ...]` by accessing each tuple field and calling inspect.
fn generate_tuple_inspect_fn(
    type_arg_names: &[String],
    elements: &[TypeId],
    tuple_type: TypeId,
    ref_tuple_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        "Tuple".to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    )
    .with_struct_type_args(type_arg_names);
    let qualified_name = method_info.to_mangled_name();

    let self_ref = || local_expr(0, "self", ref_tuple_type, span);
    let deref_self = || deref_expr(self_ref(), tuple_type, span);
    let fmt = || local_expr(1, "f", fmt_type, span);

    let mut stmts = Vec::new();
    stmts.push(write_str_stmt("[", fmt(), string_type, span));
    for (i, elem_type) in elements.iter().enumerate() {
        if i > 0 {
            stmts.push(write_str_stmt(", ", fmt(), string_type, span));
        }
        stmts.push(inspect_call(
            field_access(deref_self(), i as u32, i.to_string(), *elem_type, span),
            *elem_type,
            fmt(),
            module_source,
            tt,
            span,
        ));
    }
    stmts.push(write_str_stmt("]", fmt(), string_type, span));

    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_tuple_type, fmt_type, span),
        TypeTable::UNIT,
        TirBlock::new(stmts, span),
        vec![ref_tuple_type, fmt_type],
    )
}

/// Generate `Fn<N,Ret>^Inspect::inspect(&self, &mut Formatter)` for a concrete function type.
///
/// Body: writes the function type signature, e.g., `|i32, String| -> bool`.
fn generate_fn_inspect_fn(
    type_arg_names: &[String],
    param_types: &[TypeId],
    return_type: TypeId,
    _fn_type: TypeId,
    ref_fn_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = LocalMethodName::new(
        "Fn".to_string(),
        Some("Inspect".to_string()),
        "inspect".to_string(),
    )
    .with_struct_type_args(type_arg_names);
    let qualified_name = method_info.to_mangled_name();

    // Build the signature string at compile time: "|i32, String| -> bool"
    let param_names: Vec<String> = param_types.iter().map(|t| tt.type_name(*t)).collect();
    let ret_name = tt.type_name(return_type);
    let sig = format!("|{}| -> {}", param_names.join(", "), ret_name);

    let fmt = || local_expr(1, "f", fmt_type, span);
    let body = TirBlock::new(vec![write_str_stmt(sig, fmt(), string_type, span)], span);

    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_fn_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        vec![ref_fn_type, fmt_type],
    )
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
        vec![ref_type, fmt_type],
    )
}

/// Standard parameters for `Inspect::inspect` and `Display::fmt`.
fn inspect_params(ref_type: TypeId, fmt_type: TypeId, span: Span) -> Vec<TirParam> {
    vec![
        TirParam {
            name: "self".to_string(),
            type_id: ref_type,
            local_index: 0,
            span,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            span,
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

    // Helper: check if a Display fallback should be generated.
    // Returns true if no Display exists AND an Inspect impl exists.
    let should_generate = |display_key: &str, inspect_key: &str| -> bool {
        if existing.contains(display_key) {
            return false;
        }
        existing.contains(inspect_key) || all_fn_names.contains(inspect_key)
    };

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
        let (display_key, inspect_key) = (
            MethodName::format_local(&name, Some("Display"), "fmt"),
            MethodName::format_local(&name, Some("Inspect"), "inspect"),
        );
        if !should_generate(&display_key, &inspect_key) {
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
        let (display_key, inspect_key) = (
            MethodName::format_local(&name, Some("Display"), "fmt"),
            MethodName::format_local(&name, Some("Inspect"), "inspect"),
        );
        if !should_generate(&display_key, &inspect_key) {
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
        let (display_key, inspect_key) = (
            MethodName::format_local(&name, Some("Display"), "fmt"),
            MethodName::format_local(&name, Some("Inspect"), "inspect"),
        );
        if !should_generate(&display_key, &inspect_key) {
            continue;
        }
        let type_param_ids: Vec<TypeId> = type_params
            .iter()
            .map(|tp| tt.make_type_param(tp.name.clone(), tp.index))
            .collect();
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
        let (display_key, inspect_key) = (
            MethodName::format_local(&name, Some("Display"), "fmt"),
            MethodName::format_local(&name, Some("Inspect"), "inspect"),
        );
        if !should_generate(&display_key, &inspect_key) {
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
        let (display_key, inspect_key) = (
            MethodName::format_local(&name, Some("Display"), "fmt"),
            MethodName::format_local(&name, Some("Inspect"), "inspect"),
        );
        if !should_generate(&display_key, &inspect_key) {
            continue;
        }
        let type_param_ids: Vec<TypeId> = type_params
            .iter()
            .map(|tp| tt.make_type_param(tp.name.clone(), tp.index))
            .collect();
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
        let (display_key, inspect_key) = (
            MethodName::format_local(&name, Some("Display"), "fmt"),
            MethodName::format_local(&name, Some("Inspect"), "inspect"),
        );
        if !should_generate(&display_key, &inspect_key) {
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
        let (display_key, inspect_key) = (
            MethodName::format_local(&nt.name, Some("Display"), "fmt"),
            MethodName::format_local(&nt.name, Some("Inspect"), "inspect"),
        );
        if !should_generate(&display_key, &inspect_key) {
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

    // Parameterized types (tuples, function types) — Display fallback
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        let mangled = format_parameterized_name(&base_name, &type_arg_names);
        let display_key = format!("{mangled}^Display::fmt");
        let inspect_key = format!("{mangled}^Inspect::inspect");
        if !should_generate(&display_key, &inspect_key) {
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
            span,
        },
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            span,
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
        effects: Vec::new(),
        body: Some(body),
        span,
        local_count: 2,
        local_types: vec![ref_type, fmt_type],
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
    }
}

/// Build a `value.inspect(f)` method call statement.
///
/// Resolves the value type to determine the correct Inspect impl to call:
/// - Type parameters (`T`): `is_type_param_receiver: true`, monomorphizer substitutes
/// - Parameterized types (`Array<T>`, `Fn<1,U>`): base name + type args
/// - Concrete types (`i32`, `Point`): direct call
fn inspect_call(
    value: TirExpr,
    value_type: TypeId,
    fmt: TirExpr,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirStmt {
    let ref_type = tt.make_ref(value_type);
    let receiver = ref_expr(value, ref_type, span);

    // Strip references to get the inner type for name mangling.
    let inner_type = strip_refs(value_type, tt);

    // Decompose the type into (base_name, is_type_param, type_arg_names).
    // All parameterized types must be explicitly listed to avoid producing names
    // with `<` that would cause `LocalMethodName::new` to panic.
    let resolved = tt.get(inner_type).clone();
    let (base_name, is_type_param, type_arg_names) =
        decompose_type_for_method_name(&resolved, inner_type, tt);

    let mut info = LocalMethodName::new(
        base_name,
        Some("Inspect".to_string()),
        "inspect".to_string(),
    );
    if !type_arg_names.is_empty() {
        info = info.with_struct_type_args(&type_arg_names);
    }
    info.is_type_param_receiver = is_type_param;

    let impl_module = if is_type_param {
        module_source.clone()
    } else {
        inspect_impl_module(value_type, tt, module_source)
    };

    trait_method_call(receiver, info, impl_module, vec![fmt], span)
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
        ResolvedType::Tuple(elems) => {
            let args = elems.iter().map(|t| tt.mangle_type_name(*t)).collect();
            ("Tuple".to_string(), false, args)
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
        ResolvedType::Reactive(inner) => {
            ("Reactive".to_string(), false, vec![tt.mangle_type_name(*inner)])
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

/// Strip all reference wrappers from a type, returning the inner type.
fn strip_refs(type_id: TypeId, tt: &TypeTable) -> TypeId {
    let mut current = type_id;
    loop {
        match tt.get(current).clone() {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => current = inner,
            _ => return current,
        }
    }
}

/// Determine the module where an Inspect impl lives for a given type.
fn inspect_impl_module(type_id: TypeId, tt: &TypeTable, default: &ModuleSource) -> ModuleSource {
    use crate::tir::ResolvedType;
    match tt.get(type_id).clone() {
        ResolvedType::Primitive(_) => ModuleSource::primitives(),
        ResolvedType::Struct { ref name, .. } if name == "String" => ModuleSource::format(),
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

/// Collect parameterized types that need Inspect/Display impls.
///
/// Returns `(type_id, base_name, type_arg_names)` for each concrete parameterized type.
/// Includes tuples, function types, and resource handle types (Future, Stream, etc.).
fn collect_parameterized_types(tt: &TypeTable) -> Vec<(TypeId, String, Vec<String>)> {
    let is_concrete = |t: TypeId| !matches!(tt.get(t), ResolvedType::TypeParam { .. });

    tt.all_types()
        .filter_map(|(id, resolved)| match resolved {
            ResolvedType::Tuple(elems) => {
                if !elems.iter().all(|e| is_concrete(*e)) {
                    return None;
                }
                let args = elems.iter().map(|e| tt.mangle_type_name(*e)).collect();
                Some((*id, "Tuple".to_string(), args))
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
            span,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_enum_type,
            local_index: 1,
            span,
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
        vec![ref_enum_type, ref_enum_type],
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
            span,
        },
        TirParam {
            name: "other".to_string(),
            type_id: ref_enum_type,
            local_index: 1,
            span,
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
        vec![ref_enum_type, ref_enum_type, enum_type, enum_type],
    )
}
