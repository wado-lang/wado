//! Serde synthesis phase.
//!
//! Generates `Serialize` and `Deserialize` trait implementations for types
//! that have `impl Trait for Type;` synthesis requests.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexSet;

use crate::name::{LocalMethodName, MethodName, ModuleSource, mangle_local_trait_method};
use crate::package::Package;
use crate::tir::{
    CallArg, FunctionRef, InlineHint, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction,
    TirMatchArm, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStructField,
    TirTemplatePart, TirTypeParam, TypeId, TypeTable,
};
use crate::token::Span;

use super::common::{
    alloc_local, block, break_stmt, cast, deref_expr, expr_stmt, field_access, i32_const, if_stmt,
    let_mut_stmt, local_ref, loop_stmt, null_expr, option_none, option_some, ref_expr, return_stmt,
    string_lit, synth_span,
};

fn apply_rename_all(s: &str, strategy: &str) -> String {
    match strategy {
        "camelCase" => snake_to_camel(s),
        "snake_case" => s.to_string(),
        "PascalCase" => {
            let mut result = String::with_capacity(s.len());
            let mut capitalize_next = true;
            for c in s.chars() {
                if c == '_' {
                    capitalize_next = true;
                } else if capitalize_next {
                    for upper in c.to_uppercase() {
                        result.push(upper);
                    }
                    capitalize_next = false;
                } else {
                    result.push(c);
                }
            }
            result
        }
        "SCREAMING_SNAKE_CASE" => s.to_uppercase(),
        "kebab-case" => s.replace('_', "-"),
        "SCREAMING-KEBAB-CASE" => s.replace('_', "-").to_uppercase(),
        _ => snake_to_camel(s), // default to camelCase
    }
}

fn snake_to_camel(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = false;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            for upper in c.to_uppercase() {
                result.push(upper);
            }
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

pub fn synthesize_serde(project: &mut Package) {
    for module in project.tir_modules.values_mut() {
        let requests: Vec<_> = module.synthesis_requests.drain(..).collect();
        if requests.is_empty() {
            continue;
        }
        let existing = collect_existing_trait_methods(module);
        let mut generated = Vec::new();

        for req in &requests {
            match req.trait_name.as_str() {
                "Serialize" => {
                    let key = MethodName::format_local(
                        &req.target_type_name,
                        Some("Serialize"),
                        "serialize",
                    );
                    if existing.contains(&key) {
                        continue;
                    }
                    let func = generate_struct_serialize(module, req)
                        .or_else(|| generate_enum_serialize(module, req))
                        .or_else(|| generate_variant_serialize(module, req))
                        .or_else(|| generate_flags_serialize(module, req));
                    if let Some(f) = func {
                        generated.push(Rc::new(RefCell::new(f)));
                    }
                }
                "Deserialize" => {
                    let key = MethodName::format_local(
                        &req.target_type_name,
                        Some("Deserialize"),
                        "deserialize",
                    );
                    if existing.contains(&key) {
                        continue;
                    }
                    if let Some((lookup_func, deser_func)) =
                        generate_struct_deserialize(module, req)
                    {
                        generated.push(Rc::new(RefCell::new(lookup_func)));
                        generated.push(Rc::new(RefCell::new(deser_func)));
                    } else {
                        let func = generate_enum_deserialize(module, req)
                            .or_else(|| generate_variant_deserialize(module, req))
                            .or_else(|| generate_flags_deserialize(module, req));
                        if let Some(f) = func {
                            generated.push(Rc::new(RefCell::new(f)));
                        }
                    }
                }
                other => {
                    panic!(
                        "unsupported synthesis trait `{other}` for `{}`",
                        req.target_type_name
                    );
                }
            }
        }

        module.functions.extend(generated);
    }
}

fn collect_existing_trait_methods(module: &TirModule) -> IndexSet<String> {
    module
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            func.method_info.as_ref().and_then(|info| {
                info.trait_name.as_ref().map(|trait_name| {
                    mangle_local_trait_method(&info.base_struct_name, trait_name, &info.method_name)
                })
            })
        })
        .collect()
}

fn find_struct<'a>(module: &'a TirModule, name: &str) -> Option<&'a crate::tir::TirStruct> {
    module.structs.iter().find(|s| s.name == name)
}

fn type_param_method_call(
    receiver: TirExpr,
    struct_name: &str,
    trait_name: &str,
    method_name: &str,
    module_source: ModuleSource,
    method_type_args: Vec<String>,
    type_args: Vec<TypeId>,
    args: Vec<TirExpr>,
    return_type: TypeId,
    span: Span,
) -> TirExpr {
    let info = if method_type_args.is_empty() {
        let mut i = LocalMethodName::new(
            struct_name.to_string(),
            Some(trait_name.to_string()),
            method_name.to_string(),
        );
        i.is_type_param_receiver = true;
        i
    } else {
        let mut i = LocalMethodName::with_method_type_args(
            struct_name.to_string(),
            Some(trait_name.to_string()),
            method_name.to_string(),
            method_type_args,
        );
        i.is_type_param_receiver = true;
        i
    };
    let fn_name = info.to_mangled_name();
    TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(receiver),
            func: FunctionRef {
                module_source,
                name: fn_name,
                monomorph_info: None,
                method_info: Some(info),
            },
            type_args,
            args: args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        },
        return_type,
        span,
    )
}

/// Create a block that extracts the Err payload from a Result and returns it
/// wrapped in a different Result type: `return Err(result.err_payload)`
fn propagate_err_block(
    result_local: u32,
    result_local_name: &str,
    result_type: TypeId,
    err_type: TypeId,
    outer_result_type: TypeId,
    span: Span,
) -> TirBlock {
    let extract_err = TirExpr::new(
        TirExprKind::VariantPayload {
            expr: Box::new(local_ref(result_local, result_local_name, result_type)),
            case_index: 1, // Err
            payload_type: err_type,
        },
        err_type,
        span,
    );
    block(vec![return_stmt(Some(variant_err(
        extract_err,
        outer_result_type,
        span,
    )))])
}

fn variant_ok(value: TirExpr, result_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantConstruct {
            variant_type: result_type,
            case_index: 0,
            case_name: "Ok".to_string(),
            payload: Some(Box::new(value)),
        },
        result_type,
        span,
    )
}

fn variant_err(value: TirExpr, result_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantConstruct {
            variant_type: result_type,
            case_index: 1,
            case_name: "Err".to_string(),
            payload: Some(Box::new(value)),
        },
        result_type,
        span,
    )
}

fn if_let_ok(
    scrutinee: TirExpr,
    result_type: TypeId,
    ok_type: TypeId,
    ok_local: u32,
    ok_name: &str,
    then_block: TirBlock,
    else_block: TirBlock,
    span: Span,
) -> TirStmt {
    TirStmt::new(
        TirStmtKind::IfLet {
            scrutinee,
            pattern: TirPattern::Variant {
                enum_type: result_type,
                variant_name: "Ok".to_string(),
                bindings: vec![TirPattern::Binding {
                    name: ok_name.to_string(),
                    local_index: ok_local,
                    type_id: ok_type,
                }],
                payload_type: ok_type,
            },
            then_block,
            else_block: Some(else_block),
        },
        span,
    )
}

fn if_let_some(
    scrutinee: TirExpr,
    option_type: TypeId,
    inner_type: TypeId,
    inner_local: u32,
    inner_name: &str,
    then_block: TirBlock,
    else_block: TirBlock,
    span: Span,
) -> TirStmt {
    TirStmt::new(
        TirStmtKind::IfLet {
            scrutinee,
            pattern: TirPattern::Variant {
                enum_type: option_type,
                variant_name: "Some".to_string(),
                bindings: vec![TirPattern::Binding {
                    name: inner_name.to_string(),
                    local_index: inner_local,
                    type_id: inner_type,
                }],
                payload_type: inner_type,
            },
            then_block,
            else_block: Some(else_block),
        },
        span,
    )
}

fn serialize_error_literal(
    error_type: TypeId,
    error_kind_type: TypeId,
    message: &str,
    string_type: TypeId,
    span: Span,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: error_type,
            struct_name: "SerializeError".to_string(),
            fields: vec![
                TirStructField {
                    name: "kind".to_string(),
                    value: TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type: error_kind_type,
                            case_index: 1,
                            case_name: "Custom".to_string(),
                        },
                        error_kind_type,
                        span,
                    ),
                    field_index: 0,
                },
                TirStructField {
                    name: "message".to_string(),
                    value: string_lit(message, string_type, span),
                    field_index: 1,
                },
            ],
        },
        error_type,
        span,
    )
}

fn deserialize_error_literal(
    error_type: TypeId,
    error_kind_type: TypeId,
    kind_name: &str,
    kind_index: u32,
    message: &str,
    string_type: TypeId,
    span: Span,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: error_type,
            struct_name: "DeserializeError".to_string(),
            fields: vec![
                TirStructField {
                    name: "kind".to_string(),
                    value: TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type: error_kind_type,
                            case_index: kind_index,
                            case_name: kind_name.to_string(),
                        },
                        error_kind_type,
                        span,
                    ),
                    field_index: 0,
                },
                TirStructField {
                    name: "message".to_string(),
                    value: string_lit(message, string_type, span),
                    field_index: 1,
                },
                TirStructField {
                    name: "offset".to_string(),
                    value: TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: u64::MAX,
                            repr: "-1".to_string(),
                        },
                        TypeTable::I64,
                        span,
                    ),
                    field_index: 2,
                },
            ],
        },
        error_type,
        span,
    )
}

/// Generate a `T::default()` static call for the field's initial value.
/// Only emits the call when the `Default` trait is registered via `#[comp_feature("default")]`.
/// The module source for resolution comes from the type itself (where `impl Default for T` lives).
fn default_value_for_type(type_id: TypeId, type_table: &TypeTable, span: Span) -> TirExpr {
    // Gate: only generate Default calls if the trait is registered via comp_feature.
    if type_table.default_trait_module_source().is_none() {
        return null_expr(type_id);
    }
    // Extract base type name and module_source from the resolved type.
    // Only generate Default::default() calls for types known to have Default impls
    // (primitives and stdlib types). User-defined structs fall back to null.
    let (base_name, module_source, type_args) = match type_table.get(type_id) {
        crate::tir::ResolvedType::Primitive(p) => {
            (p.as_str().to_string(), ModuleSource::primitive(), vec![])
        }
        crate::tir::ResolvedType::Struct {
            name,
            module_source,
            ..
        } => {
            if matches!(module_source, ModuleSource::Core { .. }) {
                (name.clone(), module_source.clone(), vec![])
            } else {
                return null_expr(type_id);
            }
        }
        crate::tir::ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
            ..
        } => (name.clone(), module_source.clone(), type_args.clone()),
        crate::tir::ResolvedType::Newtype {
            name,
            module_source,
            ..
        }
        | crate::tir::ResolvedType::Flags {
            name,
            module_source,
        } => (name.clone(), module_source.clone(), vec![]),
        _ => return null_expr(type_id),
    };
    let mut method_info = LocalMethodName::new(
        base_name,
        Some("Default".to_string()),
        "default".to_string(),
    );
    if !type_args.is_empty() {
        let arg_names: Vec<String> = type_args.iter().map(|t| type_table.type_name(*t)).collect();
        method_info = method_info.with_type_args(&arg_names, &[]);
    }
    let mangled_name = method_info.to_mangled_name();
    // For generic types (Option<String>, Array<i32>, etc.), set monomorph_info with
    // the concrete type_args so the monomorphizer substitutes them correctly instead of
    // blindly replacing with the enclosing function's type parameters.
    let monomorph_info = if type_args.is_empty() {
        None
    } else {
        Some(crate::tir::MonomorphInfo {
            generic_name: method_info.base_struct_name.clone(),
            impl_type_args: type_args,
            method_type_args: vec![],
            is_blanket: false,
        })
    };
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source,
                name: mangled_name,
                monomorph_info,
                method_info: Some(method_info),
            },
            type_args: vec![],
            args: vec![],
        },
        type_id,
        span,
    )
}

fn generate_struct_serialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<TirFunction> {
    let struct_def = find_struct(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let struct_type = req.target_type_id;
    let ref_self_type = tt.make_ref(struct_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct("SerializeError".to_string(), serde_module.clone());
    let ser_error_kind_type = tt
        .find_enum_type_by_name("SerializeErrorKind")
        .unwrap_or(TypeTable::I32);
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);
    let struct_ser_type = tt.make_assoc_type_projection(
        s_type_param,
        "StructSerializer".to_string(),
        vec!["SerializeStruct".to_string()],
        vec![],
    );
    let result_ss_err = tt.make_result(struct_ser_type, ser_error_type);
    let mut_ref_ss = tt.make_mut_ref(struct_ser_type);

    let fields: Vec<(String, String, TypeId, u32)> = struct_def
        .fields
        .iter()
        .map(|f| {
            let serialized_name = f.serde_rename.clone().unwrap_or_else(|| {
                if let Some(strategy) = &struct_def.serde_rename_all {
                    apply_rename_all(&f.name, strategy)
                } else {
                    snake_to_camel(&f.name)
                }
            });
            (f.name.clone(), serialized_name, f.type_id, f.index)
        })
        .collect();
    let field_count = fields.len();
    let field_ref_types: Vec<TypeId> = fields
        .iter()
        .map(|(_, _, type_id, _)| tt.make_ref(*type_id))
        .collect();
    let field_type_names: Vec<String> = fields
        .iter()
        .map(|(_, _, type_id, _)| tt.type_name(*type_id))
        .collect();

    drop(tt);

    let mut local_types = vec![ref_self_type, mut_ref_s];
    let mut next_local: u32 = 2;
    let result_tmp = alloc_local(&mut next_local, &mut local_types, result_ss_err);
    let st_local = alloc_local(&mut next_local, &mut local_types, struct_ser_type);

    let mut stmts = Vec::new();

    let begin_call = type_param_method_call(
        local_ref(1, "s", mut_ref_s),
        "S",
        "Serializer",
        "begin_struct",
        serde_module.clone(),
        vec![],
        vec![],
        vec![
            ref_expr(
                string_lit(&req.target_type_name, string_type, span),
                ref_string_type,
                span,
            ),
            i32_const(field_count as i32),
        ],
        result_ss_err,
        span,
    );
    stmts.push(let_mut_stmt(
        "__result",
        result_tmp,
        result_ss_err,
        begin_call,
    ));

    let mut then_stmts = Vec::new();
    for (i, (field_name, camel_name, field_type, field_index)) in fields.iter().enumerate() {
        let self_ref = local_ref(0, "self", ref_self_type);
        let self_deref = deref_expr(self_ref, struct_type, span);
        let field_val = field_access(self_deref, *field_index, field_name, *field_type, span);
        let field_ref = ref_expr(field_val, field_ref_types[i], span);

        let field_call = type_param_method_call(
            local_ref(st_local, "st", mut_ref_ss),
            "S::StructSerializer",
            "SerializeStruct",
            "field",
            serde_module.clone(),
            vec![field_type_names[i].clone()],
            vec![*field_type],
            vec![
                ref_expr(
                    string_lit(camel_name, string_type, span),
                    ref_string_type,
                    span,
                ),
                field_ref,
            ],
            result_unit_err,
            span,
        );
        then_stmts.push(expr_stmt(field_call));
    }

    let end_call = type_param_method_call(
        local_ref(st_local, "st", mut_ref_ss),
        "S::StructSerializer",
        "SerializeStruct",
        "end",
        serde_module,
        vec![],
        vec![],
        vec![],
        result_unit_err,
        span,
    );
    then_stmts.push(return_stmt(Some(end_call)));

    let err_val = serialize_error_literal(
        ser_error_type,
        ser_error_kind_type,
        "begin_struct failed",
        string_type,
        span,
    );
    let else_stmts = vec![return_stmt(Some(variant_err(
        err_val,
        result_unit_err,
        span,
    )))];

    stmts.push(if_let_ok(
        local_ref(result_tmp, "__result", result_ss_err),
        result_ss_err,
        struct_ser_type,
        st_local,
        "st",
        block(then_stmts),
        block(else_stmts),
        span,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Serialize".to_string()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Serialize"), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Serializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![
            TirParam {
                name: "self".to_string(),
                type_id: ref_self_type,
                local_index: 0,
                is_mut: false,
                span,
                default_expr: None,
            },
            TirParam {
                name: "s".to_string(),
                type_id: mut_ref_s,
                local_index: 1,
                is_mut: false,
                span,
                default_expr: None,
            },
        ],
        return_type: result_unit_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    })
}

fn generate_struct_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<(TirFunction, TirFunction)> {
    let struct_def = find_struct(module, &req.target_type_name)?;
    let span = synth_span();
    let module_source = module.module_source.clone();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let struct_type = req.target_type_id;
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let option_i32 = tt.make_option(TypeTable::I32);
    let deser_error_type = tt.make_struct("DeserializeError".to_string(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type_by_name("DeserializeErrorKind")
        .unwrap_or(TypeTable::I32);
    let result_struct_err = tt.make_result(struct_type, deser_error_type);
    let lookup_fn_type = tt.make_function(
        vec![ref_string_type, TypeTable::I32, TypeTable::I32],
        option_i32,
        vec![],
        vec![],
    );
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let struct_access_type = tt.make_assoc_type_projection(
        d_type_param,
        "StructAccess".to_string(),
        vec!["DeserializeStruct".to_string()],
        vec![],
    );
    let result_sa_err = tt.make_result(struct_access_type, deser_error_type);
    let mut_ref_sa = tt.make_mut_ref(struct_access_type);
    let result_unit_err = tt.make_result(TypeTable::UNIT, deser_error_type);
    let result_opt_i32_err = tt.make_result(option_i32, deser_error_type);

    let fields: Vec<(String, String, TypeId, u32)> = struct_def
        .fields
        .iter()
        .map(|f| {
            let serialized_name = f.serde_rename.clone().unwrap_or_else(|| {
                if let Some(strategy) = &struct_def.serde_rename_all {
                    apply_rename_all(&f.name, strategy)
                } else {
                    snake_to_camel(&f.name)
                }
            });
            (f.name.clone(), serialized_name, f.type_id, f.index)
        })
        .collect();
    let field_count = fields.len();
    let field_result_types: Vec<TypeId> = fields
        .iter()
        .map(|(_, _, type_id, _)| tt.make_result(*type_id, deser_error_type))
        .collect();
    let field_type_names: Vec<String> = fields
        .iter()
        .map(|(_, _, type_id, _)| tt.type_name(*type_id))
        .collect();

    drop(tt);

    let lookup_func = generate_lookup_function(
        &req.target_type_name,
        &fields,
        string_type,
        ref_string_type,
        option_i32,
        span,
    );

    let lookup_fn_name = format!("_{}_field_lookup", req.target_type_name.to_lowercase());

    let mut local_types = vec![mut_ref_d];
    let mut next_local: u32 = 1;
    let result_tmp = alloc_local(&mut next_local, &mut local_types, result_sa_err);
    let sd_local = alloc_local(&mut next_local, &mut local_types, struct_access_type);
    let seen_local = alloc_local(&mut next_local, &mut local_types, TypeTable::U32);
    let field_locals: Vec<u32> = fields
        .iter()
        .map(|(_, _, type_id, _)| alloc_local(&mut next_local, &mut local_types, *type_id))
        .collect();

    let mut stmts = Vec::new();

    let lookup_closure = TirExpr::new(
        TirExprKind::Closure {
            params: vec![
                ("__input".to_string(), ref_string_type),
                ("__start".to_string(), TypeTable::I32),
                ("__end".to_string(), TypeTable::I32),
            ],
            body: Box::new(TirExpr::new(
                TirExprKind::Call {
                    func: FunctionRef {
                        module_source,
                        name: lookup_fn_name,
                        monomorph_info: None,
                        method_info: None,
                    },
                    type_args: vec![],
                    args: vec![
                        CallArg::new(local_ref(0, "__input", ref_string_type), false),
                        CallArg::new(local_ref(1, "__start", TypeTable::I32), false),
                        CallArg::new(local_ref(2, "__end", TypeTable::I32), false),
                    ],
                },
                option_i32,
                span,
            )),
            captures: vec![],
            functor_id: None,
            source_text: None,
        },
        lookup_fn_type,
        span,
    );
    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        "Deserializer",
        "begin_struct",
        serde_module.clone(),
        vec![],
        vec![],
        vec![
            ref_expr(
                string_lit(&req.target_type_name, string_type, span),
                ref_string_type,
                span,
            ),
            i32_const(field_count as i32),
            lookup_closure,
        ],
        result_sa_err,
        span,
    );
    stmts.push(let_mut_stmt(
        "__result",
        result_tmp,
        result_sa_err,
        begin_call,
    ));

    let mut then_stmts = Vec::new();

    then_stmts.push(let_mut_stmt(
        "seen",
        seen_local,
        TypeTable::U32,
        TirExpr::new(
            TirExprKind::IntLiteral {
                value: 0,
                repr: "0".to_string(),
            },
            TypeTable::U32,
            span,
        ),
    ));

    {
        let tt = module.type_table.borrow();
        for (i, (field_name, _, type_id, _)) in fields.iter().enumerate() {
            // Prefer the field's declared default expression (WEP 2026-04-11)
            // when available; fall back to `T::default()` for types with an
            // auto-derived Default impl; otherwise null.
            let default_val = struct_def
                .fields
                .get(i)
                .and_then(|f| f.default_expr.as_ref())
                .map(|e| (**e).clone())
                .unwrap_or_else(|| default_value_for_type(*type_id, &tt, span));
            then_stmts.push(let_mut_stmt(
                field_name,
                field_locals[i],
                *type_id,
                default_val,
            ));
        }
    }

    // Build loop body
    let next_result_local = alloc_local(&mut next_local, &mut local_types, result_opt_i32_err);
    let next_opt_local = alloc_local(&mut next_local, &mut local_types, option_i32);
    let field_idx_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);

    let mut loop_stmts = Vec::new();

    let next_field_call = type_param_method_call(
        local_ref(sd_local, "sd", mut_ref_sa),
        "D::StructAccess",
        "DeserializeStruct",
        "next_field",
        serde_module.clone(),
        vec![],
        vec![],
        vec![],
        result_opt_i32_err,
        span,
    );
    loop_stmts.push(let_mut_stmt(
        "__next",
        next_result_local,
        result_opt_i32_err,
        next_field_call,
    ));

    // Build match arms
    let mut match_arms = Vec::new();
    for (i, (field_name, _, type_id, _)) in fields.iter().enumerate() {
        let bit = 1u32 << i;
        let value_call = type_param_method_call(
            local_ref(sd_local, "sd", mut_ref_sa),
            "D::StructAccess",
            "DeserializeStruct",
            "value",
            serde_module.clone(),
            vec![field_type_names[i].clone()],
            vec![*type_id],
            vec![],
            field_result_types[i],
            span,
        );
        let val_result_local =
            alloc_local(&mut next_local, &mut local_types, field_result_types[i]);
        let val_ok_local = alloc_local(&mut next_local, &mut local_types, *type_id);

        let assign_block = block(vec![
            expr_stmt(TirExpr::new(
                TirExprKind::Assign {
                    target: Box::new(local_ref(field_locals[i], field_name, *type_id)),
                    value: Box::new(local_ref(val_ok_local, "__val", *type_id)),
                },
                TypeTable::UNIT,
                span,
            )),
            expr_stmt(TirExpr::new(
                TirExprKind::Assign {
                    target: Box::new(local_ref(seen_local, "seen", TypeTable::U32)),
                    value: Box::new(TirExpr::new(
                        TirExprKind::Binary {
                            op: crate::tir::TirBinaryOp::BitOr,
                            left: Box::new(local_ref(seen_local, "seen", TypeTable::U32)),
                            right: Box::new(TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: u64::from(bit),
                                    repr: bit.to_string(),
                                },
                                TypeTable::U32,
                                span,
                            )),
                        },
                        TypeTable::U32,
                        span,
                    )),
                },
                TypeTable::UNIT,
                span,
            )),
        ]);

        let arm_stmts = vec![
            let_mut_stmt("__vr", val_result_local, field_result_types[i], value_call),
            if_let_ok(
                local_ref(val_result_local, "__vr", field_result_types[i]),
                field_result_types[i],
                *type_id,
                val_ok_local,
                "__val",
                assign_block,
                propagate_err_block(
                    val_result_local,
                    "__vr",
                    field_result_types[i],
                    deser_error_type,
                    result_struct_err,
                    span,
                ),
                span,
            ),
        ];

        match_arms.push(TirMatchArm {
            pattern: TirPattern::Literal(crate::tir::TirLiteralPattern::I128(i as i128)),
            guard: None,
            body: TirExpr::new(TirExprKind::Block(block(arm_stmts)), TypeTable::UNIT, span),
            span,
        });
    }

    let skip_call = type_param_method_call(
        local_ref(sd_local, "sd", mut_ref_sa),
        "D::StructAccess",
        "DeserializeStruct",
        "skip",
        serde_module.clone(),
        vec![],
        vec![],
        vec![],
        result_unit_err,
        span,
    );
    match_arms.push(TirMatchArm {
        pattern: TirPattern::Wildcard,
        guard: None,
        body: TirExpr::new(
            TirExprKind::Block(block(vec![expr_stmt(skip_call)])),
            TypeTable::UNIT,
            span,
        ),
        span,
    });

    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(local_ref(field_idx_local, "__idx", TypeTable::I32)),
            arms: match_arms,
        },
        TypeTable::UNIT,
        span,
    );

    let if_some = if_let_some(
        local_ref(next_opt_local, "__opt", option_i32),
        option_i32,
        TypeTable::I32,
        field_idx_local,
        "__idx",
        block(vec![expr_stmt(match_expr)]),
        block(vec![break_stmt()]),
        span,
    );

    let if_ok = if_let_ok(
        local_ref(next_result_local, "__next", result_opt_i32_err),
        result_opt_i32_err,
        option_i32,
        next_opt_local,
        "__opt",
        block(vec![if_some]),
        propagate_err_block(
            next_result_local,
            "__next",
            result_opt_i32_err,
            deser_error_type,
            result_struct_err,
            span,
        ),
        span,
    );
    loop_stmts.push(if_ok);

    then_stmts.push(loop_stmt(block(loop_stmts.clone())));

    // Check seen mask — only require non-default fields
    if field_count > 0 {
        let mut required_mask = 0u32;
        for (i, f) in struct_def.fields.iter().enumerate() {
            if !f.serde_default {
                required_mask |= 1u32 << i;
            }
        }
        if required_mask != 0 {
            let ne_check = TirExpr::new(
                TirExprKind::Binary {
                    op: crate::tir::TirBinaryOp::NotEq,
                    left: Box::new(TirExpr::new(
                        TirExprKind::Binary {
                            op: crate::tir::TirBinaryOp::BitAnd,
                            left: Box::new(local_ref(seen_local, "seen", TypeTable::U32)),
                            right: Box::new(TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: u64::from(required_mask),
                                    repr: required_mask.to_string(),
                                },
                                TypeTable::U32,
                                span,
                            )),
                        },
                        TypeTable::U32,
                        span,
                    )),
                    right: Box::new(TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: u64::from(required_mask),
                            repr: required_mask.to_string(),
                        },
                        TypeTable::U32,
                        span,
                    )),
                },
                TypeTable::BOOL,
                span,
            );
            let missing_err = deserialize_error_literal(
                deser_error_type,
                deser_error_kind_type,
                "MissingField",
                1,
                "required field missing",
                string_type,
                span,
            );
            then_stmts.push(if_stmt(
                ne_check,
                block(vec![return_stmt(Some(variant_err(
                    missing_err,
                    result_struct_err,
                    span,
                )))]),
                None,
            ));
        }
    }

    // sd.end()
    let end_call = type_param_method_call(
        local_ref(sd_local, "sd", mut_ref_sa),
        "D::StructAccess",
        "DeserializeStruct",
        "end",
        serde_module,
        vec![],
        vec![],
        vec![],
        result_unit_err,
        span,
    );
    then_stmts.push(expr_stmt(end_call));

    // return Ok(StructName { ... })
    let struct_fields: Vec<TirStructField> = fields
        .iter()
        .enumerate()
        .map(|(i, (name, _, type_id, index))| TirStructField {
            name: name.clone(),
            value: local_ref(field_locals[i], name, *type_id),
            field_index: *index,
        })
        .collect();
    let struct_lit = TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type,
            struct_name: req.target_type_name.clone(),
            fields: struct_fields,
        },
        struct_type,
        span,
    );
    then_stmts.push(return_stmt(Some(variant_ok(
        struct_lit,
        result_struct_err,
        span,
    ))));

    // else block
    let begin_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        "Custom",
        9,
        "begin_struct failed",
        string_type,
        span,
    );
    let else_block = block(vec![return_stmt(Some(variant_err(
        begin_err,
        result_struct_err,
        span,
    )))]);

    stmts.push(if_let_ok(
        local_ref(result_tmp, "__result", result_sa_err),
        result_sa_err,
        struct_access_type,
        sd_local,
        "sd",
        block(then_stmts),
        else_block,
        span,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Deserialize".to_string()),
        "deserialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Deserialize"), "deserialize");

    let deser_func = TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Deserializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![TirParam {
            name: "d".to_string(),
            type_id: mut_ref_d,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        }],
        return_type: result_struct_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    };

    Some((lookup_func, deser_func))
}

/// Build a `string.get_byte(index_expr) as i32` expression with a computed index.
fn key_get_byte_as_i32_expr(string_ref: TirExpr, index_expr: TirExpr, span: Span) -> TirExpr {
    let get_byte_method = LocalMethodName::new("String".to_string(), None, "get_byte".to_string());
    let get_byte_call = TirExpr::new(
        TirExprKind::MethodCall {
            receiver: Box::new(string_ref),
            func: FunctionRef {
                module_source: ModuleSource::string(),
                name: get_byte_method.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(get_byte_method),
            },
            type_args: vec![],
            args: vec![CallArg::new(index_expr, false)],
        },
        TypeTable::U8,
        span,
    );
    TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(get_byte_call),
            target_type: TypeTable::I32,
        },
        TypeTable::I32,
        span,
    )
}

/// Build `left && right` expression.
fn and_expr(left: TirExpr, right: TirExpr, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(left),
            op: TirBinaryOp::And,
            right: Box::new(right),
        },
        TypeTable::BOOL,
        span,
    )
}

/// Build `left == right` expression for i32 operands.
fn i32_eq(left: TirExpr, right: TirExpr, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(left),
            op: TirBinaryOp::Eq,
            right: Box::new(right),
        },
        TypeTable::BOOL,
        span,
    )
}

fn generate_lookup_function(
    type_name: &str,
    fields: &[(String, String, TypeId, u32)],
    _string_type: TypeId,
    ref_string_type: TypeId,
    option_i32: TypeId,
    span: Span,
) -> TirFunction {
    let fn_name = format!("_{}_field_lookup", type_name.to_lowercase());
    // Parameters: input: &String (0), start: i32 (1), end: i32 (2)
    let mut local_types = vec![ref_string_type, TypeTable::I32, TypeTable::I32];
    let mut next_local: u32 = 3;

    let mut stmts = Vec::new();

    // Allocate a local for `let __len = end - start`
    let len_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
    let len_expr = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(local_ref(2, "__end", TypeTable::I32)),
            op: TirBinaryOp::Sub,
            right: Box::new(local_ref(1, "__start", TypeTable::I32)),
        },
        TypeTable::I32,
        span,
    );
    stmts.push(let_mut_stmt("__len", len_local, TypeTable::I32, len_expr));

    // For each field, generate:
    //   if __len == N && input.get_byte(start + 0) as i32 == B0 && ... { return Some(i); }
    for (i, (_, camel_name, _, _)) in fields.iter().enumerate() {
        let name_bytes = camel_name.as_bytes();
        let name_len = name_bytes.len() as i32;

        // Start with: __len == name_len
        let mut condition = i32_eq(
            local_ref(len_local, "__len", TypeTable::I32),
            i32_const(name_len),
            span,
        );

        // Chain: && input.get_byte(start + j) as i32 == byte_j
        for (j, &byte_val) in name_bytes.iter().enumerate() {
            // start + j
            let index_expr = TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(local_ref(1, "__start", TypeTable::I32)),
                    op: TirBinaryOp::Add,
                    right: Box::new(i32_const(j as i32)),
                },
                TypeTable::I32,
                span,
            );
            let byte_check = i32_eq(
                key_get_byte_as_i32_expr(
                    local_ref(0, "__input", ref_string_type),
                    index_expr,
                    span,
                ),
                i32_const(i32::from(byte_val)),
                span,
            );
            condition = and_expr(condition, byte_check, span);
        }

        stmts.push(if_stmt(
            condition,
            block(vec![return_stmt(Some(option_some(
                i32_const(i as i32),
                option_i32,
            )))]),
            None,
        ));
    }
    stmts.push(return_stmt(Some(option_none(option_i32))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: fn_name,
        is_pub: false,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: vec![
            TirParam {
                name: "__input".to_string(),
                type_id: ref_string_type,
                local_index: 0,
                is_mut: false,
                span,
                default_expr: None,
            },
            TirParam {
                name: "__start".to_string(),
                type_id: TypeTable::I32,
                local_index: 1,
                is_mut: false,
                span,
                default_expr: None,
            },
            TirParam {
                name: "__end".to_string(),
                type_id: TypeTable::I32,
                local_index: 2,
                is_mut: false,
                span,
                default_expr: None,
            },
        ],
        return_type: option_i32,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    }
}

fn find_enum<'a>(module: &'a TirModule, name: &str) -> Option<&'a crate::tir::TirEnum> {
    module.enums.iter().find(|e| e.name == name)
}

fn find_variant<'a>(module: &'a TirModule, name: &str) -> Option<&'a crate::tir::TirVariantDecl> {
    module.variants.iter().find(|v| v.name == name)
}

fn generate_enum_serialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<TirFunction> {
    let enum_def = find_enum(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let enum_type = req.target_type_id;
    let ref_self_type = tt.make_ref(enum_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct("SerializeError".to_string(), serde_module.clone());
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);

    let cases: Vec<(String, u32)> = enum_def
        .cases
        .iter()
        .map(|c| (c.name.clone(), c.index))
        .collect();

    drop(tt);

    let local_types = vec![ref_self_type, mut_ref_s];
    let next_local: u32 = 2;

    // Build match arms: one per enum case calling serialize_unit_variant
    let mut match_arms = Vec::new();
    for (case_name, case_index) in &cases {
        let call = type_param_method_call(
            local_ref(1, "s", mut_ref_s),
            "S",
            "Serializer",
            "serialize_unit_variant",
            serde_module.clone(),
            vec![],
            vec![],
            vec![
                ref_expr(
                    string_lit(&req.target_type_name, string_type, span),
                    ref_string_type,
                    span,
                ),
                ref_expr(
                    string_lit(case_name, string_type, span),
                    ref_string_type,
                    span,
                ),
                i32_const(*case_index as i32),
            ],
            result_unit_err,
            span,
        );
        match_arms.push(TirMatchArm {
            pattern: TirPattern::Enum {
                enum_type,
                case_name: case_name.clone(),
                case_index: *case_index,
            },
            guard: None,
            body: call,
            span,
        });
    }

    let self_deref = deref_expr(local_ref(0, "self", ref_self_type), enum_type, span);
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(self_deref),
            arms: match_arms,
        },
        result_unit_err,
        span,
    );

    let stmts = vec![return_stmt(Some(match_expr))];

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Serialize".to_string()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Serialize"), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Serializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![
            TirParam {
                name: "self".to_string(),
                type_id: ref_self_type,
                local_index: 0,
                is_mut: false,
                span,
                default_expr: None,
            },
            TirParam {
                name: "s".to_string(),
                type_id: mut_ref_s,
                local_index: 1,
                is_mut: false,
                span,
                default_expr: None,
            },
        ],
        return_type: result_unit_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    })
}

fn generate_enum_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<TirFunction> {
    let enum_def = find_enum(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let enum_type = req.target_type_id;
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let deser_error_type = tt.make_struct("DeserializeError".to_string(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type_by_name("DeserializeErrorKind")
        .unwrap_or(TypeTable::I32);
    let result_enum_err = tt.make_result(enum_type, deser_error_type);
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let variant_access_type = tt.make_assoc_type_projection(
        d_type_param,
        "VariantAccess".to_string(),
        vec!["DeserializeVariant".to_string()],
        vec![],
    );
    let result_va_err = tt.make_result(variant_access_type, deser_error_type);
    let mut_ref_va = tt.make_mut_ref(variant_access_type);
    let result_string_err = tt.make_result(string_type, deser_error_type);
    let result_unit_err = tt.make_result(TypeTable::UNIT, deser_error_type);

    let cases: Vec<(String, u32)> = enum_def
        .cases
        .iter()
        .map(|c| (c.name.clone(), c.index))
        .collect();
    let num_cases = cases.len();

    drop(tt);

    let result_i32_err = {
        let mut tt = module.type_table.borrow_mut();
        tt.make_result(TypeTable::I32, deser_error_type)
    };

    let mut local_types = vec![mut_ref_d];
    let mut next_local: u32 = 1;
    let va_result_local = alloc_local(&mut next_local, &mut local_types, result_va_err);
    let va_local = alloc_local(&mut next_local, &mut local_types, variant_access_type);
    let disc_result_local = alloc_local(&mut next_local, &mut local_types, result_i32_err);
    let disc_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
    let name_result_local = alloc_local(&mut next_local, &mut local_types, result_string_err);
    let name_local = alloc_local(&mut next_local, &mut local_types, string_type);

    let mut stmts = Vec::new();

    // let __va_r = d.begin_variant(&"TypeName", num_cases)
    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        "Deserializer",
        "begin_variant",
        serde_module.clone(),
        vec![],
        vec![],
        vec![
            ref_expr(
                string_lit(&req.target_type_name, string_type, span),
                ref_string_type,
                span,
            ),
            i32_const(num_cases as i32),
        ],
        result_va_err,
        span,
    );
    stmts.push(let_mut_stmt(
        "__va_r",
        va_result_local,
        result_va_err,
        begin_call,
    ));

    // if let Ok(mut va) = __va_r { ... }
    let mut then_stmts = Vec::new();

    // --- disc-based path (tried first) ---
    // let __disc_r = va.disc()
    let disc_call = type_param_method_call(
        local_ref(va_local, "va", mut_ref_va),
        "D::VariantAccess",
        "DeserializeVariant",
        "disc",
        serde_module.clone(),
        vec![],
        vec![],
        vec![],
        result_i32_err,
        span,
    );
    then_stmts.push(let_mut_stmt(
        "__disc_r",
        disc_result_local,
        result_i32_err,
        disc_call,
    ));

    // Build disc-based matching: if __disc == 0 { ... } if __disc == 1 { ... } ...
    let mut disc_then_stmts = Vec::new();
    for (case_name, case_index) in &cases {
        let condition = TirExpr::new(
            TirExprKind::Binary {
                op: crate::tir::TirBinaryOp::Eq,
                left: Box::new(local_ref(disc_local, "__disc", TypeTable::I32)),
                right: Box::new(i32_const(*case_index as i32)),
            },
            TypeTable::BOOL,
            span,
        );

        let end_call = type_param_method_call(
            local_ref(va_local, "va", mut_ref_va),
            "D::VariantAccess",
            "DeserializeVariant",
            "end",
            serde_module.clone(),
            vec![],
            vec![],
            vec![],
            result_unit_err,
            span,
        );

        let enum_construct = TirExpr::new(
            TirExprKind::EnumConstruct {
                enum_type,
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            enum_type,
            span,
        );

        let if_body = block(vec![
            expr_stmt(end_call),
            return_stmt(Some(variant_ok(enum_construct, result_enum_err, span))),
        ]);
        disc_then_stmts.push(if_stmt(condition, if_body, None));
    }

    // Unknown disc error
    let disc_unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        "UnknownVariant",
        2,
        "unknown variant discriminant",
        string_type,
        span,
    );
    disc_then_stmts.push(return_stmt(Some(variant_err(
        disc_unknown_err,
        result_enum_err,
        span,
    ))));

    // if let Ok(__disc) = __disc_r { disc_matching } else { name fallback }

    // --- name-based fallback (existing logic) ---
    let mut name_fallback_stmts = Vec::new();

    // let __name_r = va.variant_name()
    let name_call = type_param_method_call(
        local_ref(va_local, "va", mut_ref_va),
        "D::VariantAccess",
        "DeserializeVariant",
        "variant_name",
        serde_module.clone(),
        vec![],
        vec![],
        vec![],
        result_string_err,
        span,
    );
    name_fallback_stmts.push(let_mut_stmt(
        "__name_r",
        name_result_local,
        result_string_err,
        name_call,
    ));

    let mut name_then_stmts = Vec::new();

    // For each case: if name == "CaseName" { ... end(); return Ok(EnumConstruct) }
    for (case_name, case_index) in &cases {
        let key_ref = ref_expr(
            local_ref(name_local, "__name", string_type),
            ref_string_type,
            span,
        );
        let lit_ref = ref_expr(
            string_lit(case_name, string_type, span),
            ref_string_type,
            span,
        );
        let eq_method = LocalMethodName::new(
            "String".to_string(),
            Some("Eq".to_string()),
            "eq".to_string(),
        );
        let condition = TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(key_ref),
                func: FunctionRef {
                    module_source: ModuleSource::string(),
                    name: eq_method.to_mangled_name(),
                    monomorph_info: None,
                    method_info: Some(eq_method),
                },
                type_args: vec![],
                args: vec![CallArg::new(lit_ref, false)],
            },
            TypeTable::BOOL,
            span,
        );

        let end_call = type_param_method_call(
            local_ref(va_local, "va", mut_ref_va),
            "D::VariantAccess",
            "DeserializeVariant",
            "end",
            serde_module.clone(),
            vec![],
            vec![],
            vec![],
            result_unit_err,
            span,
        );

        let enum_construct = TirExpr::new(
            TirExprKind::EnumConstruct {
                enum_type,
                case_index: *case_index,
                case_name: case_name.clone(),
            },
            enum_type,
            span,
        );

        let if_body = block(vec![
            expr_stmt(end_call),
            return_stmt(Some(variant_ok(enum_construct, result_enum_err, span))),
        ]);
        name_then_stmts.push(if_stmt(condition, if_body, None));
    }

    // Unknown variant error
    let unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        "UnknownVariant",
        2,
        "unknown variant",
        string_type,
        span,
    );
    name_then_stmts.push(return_stmt(Some(variant_err(
        unknown_err,
        result_enum_err,
        span,
    ))));

    name_fallback_stmts.push(if_let_ok(
        local_ref(name_result_local, "__name_r", result_string_err),
        result_string_err,
        string_type,
        name_local,
        "__name",
        block(name_then_stmts),
        propagate_err_block(
            name_result_local,
            "__name_r",
            result_string_err,
            deser_error_type,
            result_enum_err,
            span,
        ),
        span,
    ));

    // Wire up disc path with name fallback in else
    then_stmts.push(if_let_ok(
        local_ref(disc_result_local, "__disc_r", result_i32_err),
        result_i32_err,
        TypeTable::I32,
        disc_local,
        "__disc",
        block(disc_then_stmts),
        block(name_fallback_stmts),
        span,
    ));

    // Wire up: if let Ok(mut va) = __va_r { then_stmts } else { propagate err }
    stmts.push(if_let_ok(
        local_ref(va_result_local, "__va_r", result_va_err),
        result_va_err,
        variant_access_type,
        va_local,
        "va",
        block(then_stmts),
        propagate_err_block(
            va_result_local,
            "__va_r",
            result_va_err,
            deser_error_type,
            result_enum_err,
            span,
        ),
        span,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Deserialize".to_string()),
        "deserialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Deserialize"), "deserialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Deserializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![TirParam {
            name: "d".to_string(),
            type_id: mut_ref_d,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        }],
        return_type: result_enum_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    })
}

fn generate_variant_serialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<TirFunction> {
    let variant_def = find_variant(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let variant_type = req.target_type_id;
    let ref_self_type = tt.make_ref(variant_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct("SerializeError".to_string(), serde_module.clone());
    let ser_error_kind_type = tt
        .find_enum_type_by_name("SerializeErrorKind")
        .unwrap_or(TypeTable::I32);
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);
    let variant_ser_type = tt.make_assoc_type_projection(
        s_type_param,
        "VariantSerializer".to_string(),
        vec!["SerializeVariant".to_string()],
        vec![],
    );
    let result_vs_err = tt.make_result(variant_ser_type, ser_error_type);
    let mut_ref_vs = tt.make_mut_ref(variant_ser_type);

    let cases: Vec<(String, u32, TypeId)> = variant_def
        .cases
        .iter()
        .map(|c| (c.name.clone(), c.index, c.payload))
        .collect();
    let payload_ref_types: Vec<TypeId> = cases
        .iter()
        .map(|(_, _, payload)| tt.make_ref(*payload))
        .collect();
    let payload_type_names: Vec<String> = cases
        .iter()
        .map(|(_, _, payload)| tt.type_name(*payload))
        .collect();

    drop(tt);

    let mut local_types = vec![ref_self_type, mut_ref_s];
    let mut next_local: u32 = 2;

    // Build match arms
    let mut match_arms = Vec::new();
    for (i, (case_name, case_index, payload_type)) in cases.iter().enumerate() {
        let is_unit = *payload_type == TypeTable::UNIT;

        if is_unit {
            // Unit case: serialize_unit_variant
            let call = type_param_method_call(
                local_ref(1, "s", mut_ref_s),
                "S",
                "Serializer",
                "serialize_unit_variant",
                serde_module.clone(),
                vec![],
                vec![],
                vec![
                    ref_expr(
                        string_lit(&req.target_type_name, string_type, span),
                        ref_string_type,
                        span,
                    ),
                    ref_expr(
                        string_lit(case_name, string_type, span),
                        ref_string_type,
                        span,
                    ),
                    i32_const(*case_index as i32),
                ],
                result_unit_err,
                span,
            );
            let body = TirExpr::new(
                TirExprKind::Block(block(vec![return_stmt(Some(call))])),
                result_unit_err,
                span,
            );
            match_arms.push(TirMatchArm {
                pattern: TirPattern::Variant {
                    enum_type: variant_type,
                    variant_name: case_name.clone(),
                    bindings: vec![],
                    payload_type: TypeTable::UNIT,
                },
                guard: None,
                body,
                span,
            });
        } else {
            // Payload case: begin_variant, payload, end
            let payload_local = alloc_local(&mut next_local, &mut local_types, *payload_type);
            let vs_result_local = alloc_local(&mut next_local, &mut local_types, result_vs_err);
            let vs_local = alloc_local(&mut next_local, &mut local_types, variant_ser_type);

            let begin_call = type_param_method_call(
                local_ref(1, "s", mut_ref_s),
                "S",
                "Serializer",
                "begin_variant",
                serde_module.clone(),
                vec![],
                vec![],
                vec![
                    ref_expr(
                        string_lit(&req.target_type_name, string_type, span),
                        ref_string_type,
                        span,
                    ),
                    ref_expr(
                        string_lit(case_name, string_type, span),
                        ref_string_type,
                        span,
                    ),
                    i32_const(*case_index as i32),
                ],
                result_vs_err,
                span,
            );

            let payload_ref = ref_expr(
                local_ref(payload_local, "__payload", *payload_type),
                payload_ref_types[i],
                span,
            );
            let payload_call = type_param_method_call(
                local_ref(vs_local, "__vs", mut_ref_vs),
                "S::VariantSerializer",
                "SerializeVariant",
                "payload",
                serde_module.clone(),
                vec![payload_type_names[i].clone()],
                vec![*payload_type],
                vec![payload_ref],
                result_unit_err,
                span,
            );

            let end_call = type_param_method_call(
                local_ref(vs_local, "__vs", mut_ref_vs),
                "S::VariantSerializer",
                "SerializeVariant",
                "end",
                serde_module.clone(),
                vec![],
                vec![],
                vec![],
                result_unit_err,
                span,
            );

            let err_val = serialize_error_literal(
                ser_error_type,
                ser_error_kind_type,
                "begin_variant failed",
                string_type,
                span,
            );

            let then_block = block(vec![expr_stmt(payload_call), return_stmt(Some(end_call))]);
            let else_block = block(vec![return_stmt(Some(variant_err(
                err_val,
                result_unit_err,
                span,
            )))]);

            let body_stmts = vec![
                let_mut_stmt("__vs_r", vs_result_local, result_vs_err, begin_call),
                if_let_ok(
                    local_ref(vs_result_local, "__vs_r", result_vs_err),
                    result_vs_err,
                    variant_ser_type,
                    vs_local,
                    "__vs",
                    then_block,
                    else_block,
                    span,
                ),
            ];

            match_arms.push(TirMatchArm {
                pattern: TirPattern::Variant {
                    enum_type: variant_type,
                    variant_name: case_name.clone(),
                    bindings: vec![TirPattern::Binding {
                        name: "__payload".to_string(),
                        local_index: payload_local,
                        type_id: *payload_type,
                    }],
                    payload_type: *payload_type,
                },
                guard: None,
                body: TirExpr::new(TirExprKind::Block(block(body_stmts)), result_unit_err, span),
                span,
            });
        }
    }

    let self_deref = deref_expr(local_ref(0, "self", ref_self_type), variant_type, span);
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(self_deref),
            arms: match_arms,
        },
        TypeTable::UNIT,
        span,
    );

    let stmts = vec![expr_stmt(match_expr)];

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Serialize".to_string()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Serialize"), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Serializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![
            TirParam {
                name: "self".to_string(),
                type_id: ref_self_type,
                local_index: 0,
                is_mut: false,
                span,
                default_expr: None,
            },
            TirParam {
                name: "s".to_string(),
                type_id: mut_ref_s,
                local_index: 1,
                is_mut: false,
                span,
                default_expr: None,
            },
        ],
        return_type: result_unit_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    })
}

fn generate_variant_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<TirFunction> {
    let variant_def = find_variant(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let variant_type = req.target_type_id;
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let deser_error_type = tt.make_struct("DeserializeError".to_string(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type_by_name("DeserializeErrorKind")
        .unwrap_or(TypeTable::I32);
    let result_variant_err = tt.make_result(variant_type, deser_error_type);
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let variant_access_type = tt.make_assoc_type_projection(
        d_type_param,
        "VariantAccess".to_string(),
        vec!["DeserializeVariant".to_string()],
        vec![],
    );
    let result_va_err = tt.make_result(variant_access_type, deser_error_type);
    let mut_ref_va = tt.make_mut_ref(variant_access_type);
    let result_string_err = tt.make_result(string_type, deser_error_type);
    let result_unit_err = tt.make_result(TypeTable::UNIT, deser_error_type);

    let cases: Vec<(String, u32, TypeId)> = variant_def
        .cases
        .iter()
        .map(|c| (c.name.clone(), c.index, c.payload))
        .collect();
    let num_cases = cases.len();
    let payload_result_types: Vec<TypeId> = cases
        .iter()
        .map(|(_, _, payload)| tt.make_result(*payload, deser_error_type))
        .collect();
    let payload_type_names: Vec<String> = cases
        .iter()
        .map(|(_, _, payload)| tt.type_name(*payload))
        .collect();

    let result_i32_err = tt.make_result(TypeTable::I32, deser_error_type);

    drop(tt);

    let mut local_types = vec![mut_ref_d];
    let mut next_local: u32 = 1;
    let va_result_local = alloc_local(&mut next_local, &mut local_types, result_va_err);
    let va_local = alloc_local(&mut next_local, &mut local_types, variant_access_type);
    let disc_result_local = alloc_local(&mut next_local, &mut local_types, result_i32_err);
    let disc_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
    let name_result_local = alloc_local(&mut next_local, &mut local_types, result_string_err);
    let name_local = alloc_local(&mut next_local, &mut local_types, string_type);

    let mut stmts = Vec::new();

    // let __va_r = d.begin_variant(&"TypeName", num_cases)
    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        "Deserializer",
        "begin_variant",
        serde_module.clone(),
        vec![],
        vec![],
        vec![
            ref_expr(
                string_lit(&req.target_type_name, string_type, span),
                ref_string_type,
                span,
            ),
            i32_const(num_cases as i32),
        ],
        result_va_err,
        span,
    );
    stmts.push(let_mut_stmt(
        "__va_r",
        va_result_local,
        result_va_err,
        begin_call,
    ));

    let mut then_stmts = Vec::new();

    // --- disc-based path (tried first) ---
    let disc_call = type_param_method_call(
        local_ref(va_local, "va", mut_ref_va),
        "D::VariantAccess",
        "DeserializeVariant",
        "disc",
        serde_module.clone(),
        vec![],
        vec![],
        vec![],
        result_i32_err,
        span,
    );
    then_stmts.push(let_mut_stmt(
        "__disc_r",
        disc_result_local,
        result_i32_err,
        disc_call,
    ));

    let mut disc_then_stmts = Vec::new();
    for (i, (case_name, case_index, payload_type)) in cases.iter().enumerate() {
        let is_unit = *payload_type == TypeTable::UNIT;

        let condition = TirExpr::new(
            TirExprKind::Binary {
                op: crate::tir::TirBinaryOp::Eq,
                left: Box::new(local_ref(disc_local, "__disc", TypeTable::I32)),
                right: Box::new(i32_const(*case_index as i32)),
            },
            TypeTable::BOOL,
            span,
        );

        if is_unit {
            let end_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                "D::VariantAccess",
                "DeserializeVariant",
                "end",
                serde_module.clone(),
                vec![],
                vec![],
                vec![],
                result_unit_err,
                span,
            );

            let construct = TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                    payload: None,
                },
                variant_type,
                span,
            );

            let if_body = block(vec![
                expr_stmt(end_call),
                return_stmt(Some(variant_ok(construct, result_variant_err, span))),
            ]);
            disc_then_stmts.push(if_stmt(condition, if_body, None));
        } else {
            let payload_local = alloc_local(&mut next_local, &mut local_types, *payload_type);
            let p_result_local =
                alloc_local(&mut next_local, &mut local_types, payload_result_types[i]);

            let payload_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                "D::VariantAccess",
                "DeserializeVariant",
                "payload",
                serde_module.clone(),
                vec![payload_type_names[i].clone()],
                vec![*payload_type],
                vec![],
                payload_result_types[i],
                span,
            );

            let end_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                "D::VariantAccess",
                "DeserializeVariant",
                "end",
                serde_module.clone(),
                vec![],
                vec![],
                vec![],
                result_unit_err,
                span,
            );

            let construct = TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                    payload: Some(Box::new(local_ref(
                        payload_local,
                        "__payload",
                        *payload_type,
                    ))),
                },
                variant_type,
                span,
            );

            let ok_block = block(vec![
                expr_stmt(end_call),
                return_stmt(Some(variant_ok(construct, result_variant_err, span))),
            ]);

            let if_body = block(vec![
                let_mut_stmt(
                    "__p_r",
                    p_result_local,
                    payload_result_types[i],
                    payload_call,
                ),
                if_let_ok(
                    local_ref(p_result_local, "__p_r", payload_result_types[i]),
                    payload_result_types[i],
                    *payload_type,
                    payload_local,
                    "__payload",
                    ok_block,
                    propagate_err_block(
                        p_result_local,
                        "__p_r",
                        payload_result_types[i],
                        deser_error_type,
                        result_variant_err,
                        span,
                    ),
                    span,
                ),
            ]);
            disc_then_stmts.push(if_stmt(condition, if_body, None));
        }
    }

    let disc_unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        "UnknownVariant",
        2,
        "unknown variant discriminant",
        string_type,
        span,
    );
    disc_then_stmts.push(return_stmt(Some(variant_err(
        disc_unknown_err,
        result_variant_err,
        span,
    ))));

    // --- name-based fallback ---
    let mut name_fallback_stmts = Vec::new();

    let name_call = type_param_method_call(
        local_ref(va_local, "va", mut_ref_va),
        "D::VariantAccess",
        "DeserializeVariant",
        "variant_name",
        serde_module.clone(),
        vec![],
        vec![],
        vec![],
        result_string_err,
        span,
    );
    name_fallback_stmts.push(let_mut_stmt(
        "__name_r",
        name_result_local,
        result_string_err,
        name_call,
    ));

    let mut name_then_stmts = Vec::new();

    for (i, (case_name, case_index, payload_type)) in cases.iter().enumerate() {
        let is_unit = *payload_type == TypeTable::UNIT;

        let key_ref = ref_expr(
            local_ref(name_local, "__name", string_type),
            ref_string_type,
            span,
        );
        let lit_ref = ref_expr(
            string_lit(case_name, string_type, span),
            ref_string_type,
            span,
        );
        let eq_method = LocalMethodName::new(
            "String".to_string(),
            Some("Eq".to_string()),
            "eq".to_string(),
        );
        let condition = TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(key_ref),
                func: FunctionRef {
                    module_source: ModuleSource::string(),
                    name: eq_method.to_mangled_name(),
                    monomorph_info: None,
                    method_info: Some(eq_method),
                },
                type_args: vec![],
                args: vec![CallArg::new(lit_ref, false)],
            },
            TypeTable::BOOL,
            span,
        );

        if is_unit {
            let end_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                "D::VariantAccess",
                "DeserializeVariant",
                "end",
                serde_module.clone(),
                vec![],
                vec![],
                vec![],
                result_unit_err,
                span,
            );

            let construct = TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                    payload: None,
                },
                variant_type,
                span,
            );

            let if_body = block(vec![
                expr_stmt(end_call),
                return_stmt(Some(variant_ok(construct, result_variant_err, span))),
            ]);
            name_then_stmts.push(if_stmt(condition, if_body, None));
        } else {
            let payload_local = alloc_local(&mut next_local, &mut local_types, *payload_type);
            let p_result_local =
                alloc_local(&mut next_local, &mut local_types, payload_result_types[i]);

            let payload_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                "D::VariantAccess",
                "DeserializeVariant",
                "payload",
                serde_module.clone(),
                vec![payload_type_names[i].clone()],
                vec![*payload_type],
                vec![],
                payload_result_types[i],
                span,
            );

            let end_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                "D::VariantAccess",
                "DeserializeVariant",
                "end",
                serde_module.clone(),
                vec![],
                vec![],
                vec![],
                result_unit_err,
                span,
            );

            let construct = TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                    payload: Some(Box::new(local_ref(
                        payload_local,
                        "__payload",
                        *payload_type,
                    ))),
                },
                variant_type,
                span,
            );

            let ok_block = block(vec![
                expr_stmt(end_call),
                return_stmt(Some(variant_ok(construct, result_variant_err, span))),
            ]);

            let if_body = block(vec![
                let_mut_stmt(
                    "__p_r",
                    p_result_local,
                    payload_result_types[i],
                    payload_call,
                ),
                if_let_ok(
                    local_ref(p_result_local, "__p_r", payload_result_types[i]),
                    payload_result_types[i],
                    *payload_type,
                    payload_local,
                    "__payload",
                    ok_block,
                    propagate_err_block(
                        p_result_local,
                        "__p_r",
                        payload_result_types[i],
                        deser_error_type,
                        result_variant_err,
                        span,
                    ),
                    span,
                ),
            ]);
            name_then_stmts.push(if_stmt(condition, if_body, None));
        }
    }

    // Unknown variant error
    let unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        "UnknownVariant",
        2,
        "unknown variant",
        string_type,
        span,
    );
    name_then_stmts.push(return_stmt(Some(variant_err(
        unknown_err,
        result_variant_err,
        span,
    ))));

    name_fallback_stmts.push(if_let_ok(
        local_ref(name_result_local, "__name_r", result_string_err),
        result_string_err,
        string_type,
        name_local,
        "__name",
        block(name_then_stmts),
        propagate_err_block(
            name_result_local,
            "__name_r",
            result_string_err,
            deser_error_type,
            result_variant_err,
            span,
        ),
        span,
    ));

    // Wire up disc path with name fallback
    then_stmts.push(if_let_ok(
        local_ref(disc_result_local, "__disc_r", result_i32_err),
        result_i32_err,
        TypeTable::I32,
        disc_local,
        "__disc",
        block(disc_then_stmts),
        block(name_fallback_stmts),
        span,
    ));

    stmts.push(if_let_ok(
        local_ref(va_result_local, "__va_r", result_va_err),
        result_va_err,
        variant_access_type,
        va_local,
        "va",
        block(then_stmts),
        propagate_err_block(
            va_result_local,
            "__va_r",
            result_va_err,
            deser_error_type,
            result_variant_err,
            span,
        ),
        span,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Deserialize".to_string()),
        "deserialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Deserialize"), "deserialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Deserializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![TirParam {
            name: "d".to_string(),
            type_id: mut_ref_d,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        }],
        return_type: result_variant_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    })
}

fn find_flags<'a>(module: &'a TirModule, name: &str) -> Option<&'a crate::tir::TirFlags> {
    module.flags.iter().find(|f| f.name == name)
}

/// Build `(*self as u32) & bitmask != 0` condition.
fn flags_bit_check(ref_self_type: TypeId, flags_type: TypeId, bitmask: u32, span: Span) -> TirExpr {
    let self_deref = deref_expr(local_ref(0, "self", ref_self_type), flags_type, span);
    let self_as_u32 = cast(self_deref, TypeTable::U32);
    let and_expr = TirExpr::new(
        TirExprKind::Binary {
            op: TirBinaryOp::BitAnd,
            left: Box::new(self_as_u32),
            right: Box::new(TirExpr::new(
                TirExprKind::IntLiteral {
                    value: u64::from(bitmask),
                    repr: bitmask.to_string(),
                },
                TypeTable::U32,
                span,
            )),
        },
        TypeTable::U32,
        span,
    );
    TirExpr::new(
        TirExprKind::Binary {
            op: TirBinaryOp::NotEq,
            left: Box::new(and_expr),
            right: Box::new(TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".to_string(),
                },
                TypeTable::U32,
                span,
            )),
        },
        TypeTable::BOOL,
        span,
    )
}

/// Generate `impl Serialize for FlagsType` — serializes as array of set flag name strings.
fn generate_flags_serialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<TirFunction> {
    let flags_def = find_flags(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let flags_type = req.target_type_id;
    let ref_self_type = tt.make_ref(flags_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct("SerializeError".to_string(), serde_module.clone());
    let ser_error_kind_type = tt
        .find_enum_type_by_name("SerializeErrorKind")
        .unwrap_or(TypeTable::I32);
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);
    let seq_ser_type = tt.make_assoc_type_projection(
        s_type_param,
        "SeqSerializer".to_string(),
        vec!["SerializeSeq".to_string()],
        vec![],
    );
    let result_seq_err = tt.make_result(seq_ser_type, ser_error_type);
    let mut_ref_seq = tt.make_mut_ref(seq_ser_type);

    let members: Vec<(String, u32)> = flags_def
        .members
        .iter()
        .map(|m| (m.name.clone(), m.bitmask))
        .collect();

    drop(tt);

    let mut local_types = vec![ref_self_type, mut_ref_s];
    let mut next_local: u32 = 2;
    let count_local = alloc_local(&mut next_local, &mut local_types, TypeTable::I32);
    let result_tmp = alloc_local(&mut next_local, &mut local_types, result_seq_err);
    let seq_local = alloc_local(&mut next_local, &mut local_types, seq_ser_type);

    let mut stmts = Vec::new();

    // let mut count: i32 = 0;
    stmts.push(let_mut_stmt(
        "count",
        count_local,
        TypeTable::I32,
        i32_const(0),
    ));

    // For each member: if bit set, count += 1
    for (_member_name, bitmask) in &members {
        let bit_check = flags_bit_check(ref_self_type, flags_type, *bitmask, span);
        let inc = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::Add,
                left: Box::new(local_ref(count_local, "count", TypeTable::I32)),
                right: Box::new(i32_const(1)),
            },
            TypeTable::I32,
            span,
        );
        let assign = TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(local_ref(count_local, "count", TypeTable::I32)),
                value: Box::new(inc),
            },
            TypeTable::UNIT,
            span,
        );
        stmts.push(if_stmt(bit_check, block(vec![expr_stmt(assign)]), None));
    }

    // let __result = s.begin_seq(count);
    let begin_call = type_param_method_call(
        local_ref(1, "s", mut_ref_s),
        "S",
        "Serializer",
        "begin_seq",
        serde_module.clone(),
        vec![],
        vec![],
        vec![local_ref(count_local, "count", TypeTable::I32)],
        result_seq_err,
        span,
    );
    stmts.push(let_mut_stmt(
        "__result",
        result_tmp,
        result_seq_err,
        begin_call,
    ));

    // Build the then-block: for each member, if bit set, seq.element::<String>(&"Name")
    let mut then_stmts = Vec::new();
    for (member_name, bitmask) in &members {
        let bit_check = flags_bit_check(ref_self_type, flags_type, *bitmask, span);
        let element_call = type_param_method_call(
            local_ref(seq_local, "seq", mut_ref_seq),
            "S::SeqSerializer",
            "SerializeSeq",
            "element",
            serde_module.clone(),
            vec!["String".to_string()],
            vec![string_type],
            vec![ref_expr(
                string_lit(member_name, string_type, span),
                ref_string_type,
                span,
            )],
            result_unit_err,
            span,
        );
        then_stmts.push(if_stmt(
            bit_check,
            block(vec![expr_stmt(element_call)]),
            None,
        ));
    }

    // return seq.end();
    let end_call = type_param_method_call(
        local_ref(seq_local, "seq", mut_ref_seq),
        "S::SeqSerializer",
        "SerializeSeq",
        "end",
        serde_module,
        vec![],
        vec![],
        vec![],
        result_unit_err,
        span,
    );
    then_stmts.push(return_stmt(Some(end_call)));

    // Else block: return Err(...)
    let err_val = serialize_error_literal(
        ser_error_type,
        ser_error_kind_type,
        "begin_seq failed",
        string_type,
        span,
    );
    let else_stmts = vec![return_stmt(Some(variant_err(
        err_val,
        result_unit_err,
        span,
    )))];

    stmts.push(if_let_ok(
        local_ref(result_tmp, "__result", result_seq_err),
        result_seq_err,
        seq_ser_type,
        seq_local,
        "seq",
        block(then_stmts),
        block(else_stmts),
        span,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Serialize".to_string()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Serialize"), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Serializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![
            TirParam {
                name: "self".to_string(),
                type_id: ref_self_type,
                local_index: 0,
                is_mut: false,
                span,
                default_expr: None,
            },
            TirParam {
                name: "s".to_string(),
                type_id: mut_ref_s,
                local_index: 1,
                is_mut: false,
                span,
                default_expr: None,
            },
        ],
        return_type: result_unit_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    })
}

/// Generate `impl Deserialize for FlagsType` — deserializes from array of flag name strings.
fn generate_flags_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
) -> Option<TirFunction> {
    let flags_def = find_flags(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::core("serde");

    let mut tt = module.type_table.borrow_mut();

    let flags_type = req.target_type_id;
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let string_type = tt.make_struct("String".to_string(), ModuleSource::string());
    let ref_string_type = tt.make_ref(string_type);
    let deser_error_type = tt.make_struct("DeserializeError".to_string(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type_by_name("DeserializeErrorKind")
        .unwrap_or(TypeTable::I32);
    let result_flags_err = tt.make_result(flags_type, deser_error_type);
    let result_unit_err = tt.make_result(TypeTable::UNIT, deser_error_type);
    let seq_access_type = tt.make_assoc_type_projection(
        d_type_param,
        "SeqAccess".to_string(),
        vec!["DeserializeSeq".to_string()],
        vec![],
    );
    let result_seq_err = tt.make_result(seq_access_type, deser_error_type);
    let mut_ref_seq = tt.make_mut_ref(seq_access_type);
    let option_string = tt.make_option(string_type);
    let result_option_string_err = tt.make_result(option_string, deser_error_type);

    let members: Vec<(String, u32)> = flags_def
        .members
        .iter()
        .map(|m| (m.name.clone(), m.bitmask))
        .collect();

    drop(tt);

    let mut local_types = vec![mut_ref_d];
    let mut next_local: u32 = 1;
    let result_seq_local = alloc_local(&mut next_local, &mut local_types, result_seq_err);
    let seq_local = alloc_local(&mut next_local, &mut local_types, seq_access_type);
    let bits_local = alloc_local(&mut next_local, &mut local_types, TypeTable::U32);
    let elem_result_local =
        alloc_local(&mut next_local, &mut local_types, result_option_string_err);
    let elem_local = alloc_local(&mut next_local, &mut local_types, option_string);
    let name_local = alloc_local(&mut next_local, &mut local_types, string_type);

    let mut stmts = Vec::new();

    // let __seq_r = d.begin_seq();
    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        "Deserializer",
        "begin_seq",
        serde_module.clone(),
        vec![],
        vec![],
        vec![],
        result_seq_err,
        span,
    );
    stmts.push(let_mut_stmt(
        "__seq_r",
        result_seq_local,
        result_seq_err,
        begin_call,
    ));

    // Build the body inside if let Ok(mut seq) = __seq_r { ... }
    let mut ok_stmts = Vec::new();

    // let mut bits: u32 = 0;
    ok_stmts.push(let_mut_stmt(
        "bits",
        bits_local,
        TypeTable::U32,
        TirExpr::new(
            TirExprKind::IntLiteral {
                value: 0,
                repr: "0".to_string(),
            },
            TypeTable::U32,
            span,
        ),
    ));

    // loop { let __elem_r = seq.next_element::<String>(); ... }
    let next_call = type_param_method_call(
        local_ref(seq_local, "seq", mut_ref_seq),
        "D::SeqAccess",
        "DeserializeSeq",
        "next_element",
        serde_module.clone(),
        vec!["String".to_string()],
        vec![string_type],
        vec![],
        result_option_string_err,
        span,
    );

    let mut loop_body_stmts = Vec::new();
    loop_body_stmts.push(let_mut_stmt(
        "__elem_r",
        elem_result_local,
        result_option_string_err,
        next_call,
    ));

    // if let Ok(elem) = __elem_r { ... } else { propagate err }
    // Inside Ok: if let Some(name) = elem { match name... } else { break }
    let mut name_match_stmts = Vec::new();
    for (member_name, bitmask) in &members {
        let key_ref = ref_expr(
            local_ref(name_local, "__name", string_type),
            ref_string_type,
            span,
        );
        let lit_ref = ref_expr(
            string_lit(member_name, string_type, span),
            ref_string_type,
            span,
        );
        let eq_method = LocalMethodName::new(
            "String".to_string(),
            Some("Eq".to_string()),
            "eq".to_string(),
        );
        let condition = TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(key_ref),
                func: FunctionRef {
                    module_source: ModuleSource::string(),
                    name: eq_method.to_mangled_name(),
                    monomorph_info: None,
                    method_info: Some(eq_method),
                },
                type_args: vec![],
                args: vec![CallArg::new(lit_ref, false)],
            },
            TypeTable::BOOL,
            span,
        );

        // bits = bits | bitmask
        let or_expr = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::BitOr,
                left: Box::new(local_ref(bits_local, "bits", TypeTable::U32)),
                right: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: u64::from(*bitmask),
                        repr: bitmask.to_string(),
                    },
                    TypeTable::U32,
                    span,
                )),
            },
            TypeTable::U32,
            span,
        );
        let assign = TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(local_ref(bits_local, "bits", TypeTable::U32)),
                value: Box::new(or_expr),
            },
            TypeTable::UNIT,
            span,
        );
        let continue_stmt = TirStmt {
            kind: TirStmtKind::Continue,
            span,
        };
        name_match_stmts.push(if_stmt(
            condition,
            block(vec![expr_stmt(assign), continue_stmt]),
            None,
        ));
    }

    // else: unknown flag error
    // Build error: "unknown flag: " + name
    let unknown_msg = TirExpr::new(
        TirExprKind::TemplateString {
            parts: vec![
                TirTemplatePart::Literal("unknown flag: ".to_string()),
                TirTemplatePart::Interpolation {
                    expr: Box::new(local_ref(name_local, "__name", string_type)),
                    format_spec: None,
                },
            ],
        },
        string_type,
        span,
    );
    let unknown_err = deserialize_error_literal_with_expr(
        deser_error_type,
        deser_error_kind_type,
        "InvalidValue",
        4,
        unknown_msg,
        span,
    );
    name_match_stmts.push(return_stmt(Some(variant_err(
        unknown_err,
        result_flags_err,
        span,
    ))));

    // if let Some(name) = elem { match... } else { break }
    let some_then = block(name_match_stmts);
    let none_else = block(vec![break_stmt()]);
    let if_let_some_stmt = if_let_some(
        local_ref(elem_local, "__elem", option_string),
        option_string,
        string_type,
        name_local,
        "__name",
        some_then,
        none_else,
        span,
    );

    // if let Ok(elem) = __elem_r { if let Some... } else { propagate err }
    let ok_elem_then = block(vec![if_let_some_stmt]);
    let ok_elem_else = propagate_err_block(
        elem_result_local,
        "__elem_r",
        result_option_string_err,
        deser_error_type,
        result_flags_err,
        span,
    );
    loop_body_stmts.push(if_let_ok(
        local_ref(elem_result_local, "__elem_r", result_option_string_err),
        result_option_string_err,
        option_string,
        elem_local,
        "__elem",
        ok_elem_then,
        ok_elem_else,
        span,
    ));

    ok_stmts.push(loop_stmt(block(loop_body_stmts)));

    // seq.end()?
    let end_call = type_param_method_call(
        local_ref(seq_local, "seq", mut_ref_seq),
        "D::SeqAccess",
        "DeserializeSeq",
        "end",
        serde_module,
        vec![],
        vec![],
        vec![],
        result_unit_err,
        span,
    );
    ok_stmts.push(expr_stmt(end_call));

    // return Ok(bits as FlagsType);
    let bits_as_flags = cast(local_ref(bits_local, "bits", TypeTable::U32), flags_type);
    ok_stmts.push(return_stmt(Some(variant_ok(
        bits_as_flags,
        result_flags_err,
        span,
    ))));

    // if let Ok(mut seq) = __seq_r { ... } else { propagate err }
    stmts.push(if_let_ok(
        local_ref(result_seq_local, "__seq_r", result_seq_err),
        result_seq_err,
        seq_access_type,
        seq_local,
        "seq",
        block(ok_stmts),
        propagate_err_block(
            result_seq_local,
            "__seq_r",
            result_seq_err,
            deser_error_type,
            result_flags_err,
            span,
        ),
        span,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some("Deserialize".to_string()),
        "deserialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some("Deserialize"), "deserialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec!["Deserializer".to_string()],
            default: None,
            index: 0,
        }],
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(method_info),
        params: vec![TirParam {
            name: "d".to_string(),
            type_id: mut_ref_d,
            local_index: 0,
            is_mut: false,
            span,
            default_expr: None,
        }],
        return_type: result_flags_err,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
    })
}

/// Create a `DeserializeError` literal with a dynamic message expression.
fn deserialize_error_literal_with_expr(
    deser_error_type: TypeId,
    deser_error_kind_type: TypeId,
    kind_name: &str,
    kind_index: u32,
    message_expr: TirExpr,
    span: Span,
) -> TirExpr {
    let kind = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: deser_error_kind_type,
            case_index: kind_index,
            case_name: kind_name.to_string(),
        },
        deser_error_kind_type,
        span,
    );
    let offset = TirExpr::new(
        TirExprKind::IntLiteral {
            value: u64::MAX, // -1 as i64
            repr: "-1".to_string(),
        },
        TypeTable::I64,
        span,
    );
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: deser_error_type,
            struct_name: "DeserializeError".to_string(),
            fields: vec![
                TirStructField {
                    name: "kind".to_string(),
                    value: kind,
                    field_index: 0,
                },
                TirStructField {
                    name: "message".to_string(),
                    value: message_expr,
                    field_index: 1,
                },
                TirStructField {
                    name: "offset".to_string(),
                    value: offset,
                    field_index: 2,
                },
            ],
        },
        deser_error_type,
        span,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snake_to_camel() {
        assert_eq!(snake_to_camel("age"), "age");
        assert_eq!(snake_to_camel("user_name"), "userName");
        assert_eq!(snake_to_camel("http_url"), "httpUrl");
        assert_eq!(snake_to_camel("first_name_last"), "firstNameLast");
        assert_eq!(snake_to_camel("a"), "a");
    }
}
