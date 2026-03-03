//! Serde synthesis phase.
//!
//! Generates `Serialize` and `Deserialize` trait implementations for types
//! that have `impl Trait for Type;` synthesis requests.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexSet;

use crate::name::{LocalMethodName, MethodName, ModuleSource, mangle_local_trait_method};
use crate::project::Project;
use crate::tir::{
    FunctionRef, InlineHint, TirBlock, TirExpr, TirExprKind, TirFunction, TirMatchArm, TirModule,
    TirParam, TirPattern, TirStmt, TirStmtKind, TirStructField, TirTypeParam, TypeId, TypeTable,
};
use crate::token::Span;

use super::common::{
    alloc_local, block, break_stmt, deref_expr, expr_stmt, field_access, i32_const, if_stmt,
    let_mut_stmt, local_ref, loop_stmt, null_expr, option_none, option_some, ref_expr, return_stmt,
    string_lit, synth_span,
};

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

pub fn synthesize_serde(project: &mut Project) {
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
                    if let Some(func) = generate_struct_serialize(module, req) {
                        generated.push(Rc::new(RefCell::new(func)));
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
            func: FunctionRef::External {
                module_source,
                name: fn_name,
                monomorph_info: None,
                method_info: Some(info),
            },
            type_args,
            args,
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
        TirStmtKind::IfPattern {
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
        TirStmtKind::IfPattern {
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

fn default_value_for_type(type_id: TypeId, string_type: TypeId, span: Span) -> TirExpr {
    if type_id == TypeTable::I8
        || type_id == TypeTable::I16
        || type_id == TypeTable::I32
        || type_id == TypeTable::I64
        || type_id == TypeTable::U8
        || type_id == TypeTable::U16
        || type_id == TypeTable::U32
        || type_id == TypeTable::U64
    {
        return TirExpr::new(
            TirExprKind::IntLiteral {
                value: 0,
                repr: "0".to_string(),
            },
            type_id,
            span,
        );
    }
    if type_id == TypeTable::F32 || type_id == TypeTable::F64 {
        return TirExpr::new(
            TirExprKind::FloatLiteral {
                value: 0.0,
                repr: "0.0".to_string(),
            },
            type_id,
            span,
        );
    }
    if type_id == TypeTable::BOOL {
        return TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, span);
    }
    if type_id == string_type {
        return string_lit("", string_type, span);
    }
    null_expr(type_id)
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
    );
    let result_ss_err = tt.make_result(struct_ser_type, ser_error_type);
    let mut_ref_ss = tt.make_mut_ref(struct_ser_type);

    let fields: Vec<(String, String, TypeId, u32)> = struct_def
        .fields
        .iter()
        .map(|f| (f.name.clone(), snake_to_camel(&f.name), f.type_id, f.index))
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
        serde_module.clone(),
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
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
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
                span,
            },
            TirParam {
                name: "s".to_string(),
                type_id: mut_ref_s,
                local_index: 1,
                span,
            },
        ],
        return_type: result_unit_err,
        effects: Vec::new(),
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
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
    let lookup_fn_type = tt.make_function(vec![ref_string_type], option_i32, vec![]);
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let struct_access_type = tt.make_assoc_type_projection(
        d_type_param,
        "StructAccess".to_string(),
        vec!["DeserializeStruct".to_string()],
    );
    let result_sa_err = tt.make_result(struct_access_type, deser_error_type);
    let mut_ref_sa = tt.make_mut_ref(struct_access_type);
    let result_unit_err = tt.make_result(TypeTable::UNIT, deser_error_type);
    let result_opt_i32_err = tt.make_result(option_i32, deser_error_type);

    let fields: Vec<(String, String, TypeId, u32)> = struct_def
        .fields
        .iter()
        .map(|f| (f.name.clone(), snake_to_camel(&f.name), f.type_id, f.index))
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
            params: vec![("key".to_string(), ref_string_type)],
            body: Box::new(TirExpr::new(
                TirExprKind::Call {
                    func: FunctionRef::External {
                        module_source,
                        name: lookup_fn_name,
                        monomorph_info: None,
                        method_info: None,
                    },
                    type_args: vec![],
                    args: vec![local_ref(0, "key", ref_string_type)],
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

    for (i, (field_name, _, type_id, _)) in fields.iter().enumerate() {
        let default_val = default_value_for_type(*type_id, string_type, span);
        then_stmts.push(let_mut_stmt(
            field_name,
            field_locals[i],
            *type_id,
            default_val,
        ));
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

    // Check seen mask
    if field_count > 0 {
        let full_mask = (1u32 << field_count) - 1;
        let ne_check = TirExpr::new(
            TirExprKind::Binary {
                op: crate::tir::TirBinaryOp::NotEq,
                left: Box::new(TirExpr::new(
                    TirExprKind::Binary {
                        op: crate::tir::TirBinaryOp::BitAnd,
                        left: Box::new(local_ref(seen_local, "seen", TypeTable::U32)),
                        right: Box::new(TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: u64::from(full_mask),
                                repr: full_mask.to_string(),
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
                        value: u64::from(full_mask),
                        repr: full_mask.to_string(),
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

    // sd.end()
    let end_call = type_param_method_call(
        local_ref(sd_local, "sd", mut_ref_sa),
        "D::StructAccess",
        "DeserializeStruct",
        "end",
        serde_module.clone(),
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
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
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
            span,
        }],
        return_type: result_struct_err,
        effects: Vec::new(),
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
    };

    Some((lookup_func, deser_func))
}

fn generate_lookup_function(
    type_name: &str,
    fields: &[(String, String, TypeId, u32)],
    string_type: TypeId,
    ref_string_type: TypeId,
    option_i32: TypeId,
    span: Span,
) -> TirFunction {
    let fn_name = format!("_{}_field_lookup", type_name.to_lowercase());
    let local_types = vec![ref_string_type];
    let next_local: u32 = 1;

    let mut stmts = Vec::new();
    for (i, (_, camel_name, _, _)) in fields.iter().enumerate() {
        // Call String^Eq::eq(&key, &"camelName") — both operands are &String
        let key_ref = local_ref(0, "key", ref_string_type);
        let lit_ref = ref_expr(
            string_lit(camel_name, string_type, span),
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
                func: FunctionRef::External {
                    module_source: ModuleSource::prelude(),
                    name: eq_method.to_mangled_name(),
                    monomorph_info: None,
                    method_info: Some(eq_method),
                },
                type_args: vec![],
                args: vec![lit_ref],
            },
            TypeTable::BOOL,
            span,
        );
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
        name: fn_name,
        is_pub: false,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: vec![TirParam {
            name: "key".to_string(),
            type_id: ref_string_type,
            local_index: 0,
            span,
        }],
        return_type: option_i32,
        effects: Vec::new(),
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        local_types,
        address_taken_locals: IndexSet::new(),
        is_cm_adapter: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
    }
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
