//! Serde synthesis phase.
//!
//! Generates `Serialize` and `Deserialize` trait implementations for types
//! that have `impl Trait for Type;` synthesis requests.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::{CompilerItem, CompilerItems};
use crate::hashmap::IndexSet;

use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName, mangle_local_trait_method};
use crate::package::Package;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, TirBinaryOp, TirBlock, TirExpr, TirExprKind,
    TirFunction, TirMatchArm, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirStructField, TirTemplatePart, TirTypeParam, TypeId, TypeTable,
};
use crate::token::Span;

/// Snapshot of every `core:serde` symbol name + case index this
/// synthesiser needs, resolved once through the `CompilerItem`
/// registry. Mirrors [`super::cm_binding::types::CmStdlibNames`]: built
/// per-call, cheap (a handful of registry hits + clones), and threaded
/// through the synthesis helpers so a stdlib rename of any of these
/// items flows through every call site without touching Rust code.
///
/// Field set is **minimal**: only the symbols this synthesiser actually
/// reads end up here. `Option::Some`'s case name is needed by
/// `if_let_some` (the pattern carries the name); `Option::None` is
/// **not** included because the only place serde constructs `None`
/// (`generate_lookup_function`) calls `option_none` directly with
/// `&CompilerItems`, never through this snapshot.
#[derive(Clone, Debug)]
pub(super) struct SerdeStdlibNames {
    pub serialize: String,
    pub deserialize: String,
    pub serializer: String,
    pub deserializer: String,
    pub serialize_struct: String,
    pub serialize_seq: String,
    pub serialize_variant: String,
    pub deserialize_struct: String,
    pub deserialize_seq: String,
    pub deserialize_variant: String,
    pub field_schema: String,
    pub serialize_error: String,
    pub serialize_error_kind: String,
    pub deserialize_error: String,
    pub deserialize_error_kind: String,
    pub ok_name: String,
    pub ok_index: u32,
    pub err_name: String,
    pub err_index: u32,
    /// `Some` case name. The index is intentionally omitted —
    /// `if_let_some` only needs the name on the pattern; `option_some`
    /// / `option_none` constructors take `&CompilerItems` and look up
    /// the index themselves.
    pub some_name: String,
    pub ser_err_custom_name: String,
    pub ser_err_custom_index: u32,
    pub deser_err_missing_field_name: String,
    pub deser_err_missing_field_index: u32,
    pub deser_err_unknown_variant_name: String,
    pub deser_err_unknown_variant_index: u32,
    pub deser_err_invalid_value_name: String,
    pub deser_err_invalid_value_index: u32,
    /// `Serializer` / `Deserializer` associated-type spellings,
    /// resolved by their **trait bound** (also looked up from the
    /// registry) rather than by source-side name. Renaming
    /// `Serializer::SeqSerializer` to `SeqWriter` — or renaming the
    /// bound `SerializeSeq` to `SeqWriterTrait` — both flow through
    /// the registry without panicking on a stale lookup key.
    pub seq_serializer_assoc: String,
    pub struct_serializer_assoc: String,
    pub variant_serializer_assoc: String,
    pub seq_access_assoc: String,
    pub struct_access_assoc: String,
    pub variant_access_assoc: String,
    /// Pre-formatted `S::<assoc>` / `D::<assoc>` projection labels used
    /// by `type_param_method_call` as the receiver's struct-name slot
    /// (`S` is the conventional `Serializer` type parameter; `D`, the
    /// `Deserializer` one). Reading these from the snapshot keeps all
    /// associated-type references in this file routed through
    /// [`CompilerItems::trait_assoc_type_name`].
    pub s_struct_serializer_proj: String,
    pub s_seq_serializer_proj: String,
    pub s_variant_serializer_proj: String,
    pub d_struct_access_proj: String,
    pub d_seq_access_proj: String,
    pub d_variant_access_proj: String,
}

impl SerdeStdlibNames {
    pub fn from_compiler_items(items: &CompilerItems) -> Self {
        let (_, _, ok_name, ok_index) = items.require_variant_case(CompilerItem::ResultOk);
        let (_, _, err_name, err_index) = items.require_variant_case(CompilerItem::ResultErr);
        let some_name = items
            .variant_case_name(CompilerItem::OptionSome)
            .to_string();
        let (_, _, ser_err_custom_name, ser_err_custom_index) =
            items.require_enum_case(CompilerItem::SerializeErrorKindCustom);
        let (_, _, deser_err_missing_field_name, deser_err_missing_field_index) =
            items.require_enum_case(CompilerItem::DeserializeErrorKindMissingField);
        let (_, _, deser_err_unknown_variant_name, deser_err_unknown_variant_index) =
            items.require_enum_case(CompilerItem::DeserializeErrorKindUnknownVariant);
        let (_, _, deser_err_invalid_value_name, deser_err_invalid_value_index) =
            items.require_enum_case(CompilerItem::DeserializeErrorKindInvalidValue);
        Self {
            serialize: items.trait_name(CompilerItem::Serialize).to_string(),
            deserialize: items.trait_name(CompilerItem::Deserialize).to_string(),
            serializer: items.trait_name(CompilerItem::Serializer).to_string(),
            deserializer: items.trait_name(CompilerItem::Deserializer).to_string(),
            serialize_struct: items.trait_name(CompilerItem::SerializeStruct).to_string(),
            serialize_seq: items.trait_name(CompilerItem::SerializeSeq).to_string(),
            serialize_variant: items.trait_name(CompilerItem::SerializeVariant).to_string(),
            deserialize_struct: items
                .trait_name(CompilerItem::DeserializeStruct)
                .to_string(),
            deserialize_seq: items.trait_name(CompilerItem::DeserializeSeq).to_string(),
            deserialize_variant: items
                .trait_name(CompilerItem::DeserializeVariant)
                .to_string(),
            field_schema: items.trait_name(CompilerItem::FieldSchema).to_string(),
            serialize_error: items.struct_name(CompilerItem::SerializeError).to_string(),
            serialize_error_kind: items
                .enum_name(CompilerItem::SerializeErrorKind)
                .to_string(),
            deserialize_error: items
                .struct_name(CompilerItem::DeserializeError)
                .to_string(),
            deserialize_error_kind: items
                .enum_name(CompilerItem::DeserializeErrorKind)
                .to_string(),
            ok_name: ok_name.to_string(),
            ok_index,
            err_name: err_name.to_string(),
            err_index,
            some_name,
            ser_err_custom_name: ser_err_custom_name.to_string(),
            ser_err_custom_index,
            deser_err_missing_field_name: deser_err_missing_field_name.to_string(),
            deser_err_missing_field_index,
            deser_err_unknown_variant_name: deser_err_unknown_variant_name.to_string(),
            deser_err_unknown_variant_index,
            deser_err_invalid_value_name: deser_err_invalid_value_name.to_string(),
            deser_err_invalid_value_index,
            seq_serializer_assoc: items
                .trait_assoc_type_by_bound(
                    CompilerItem::Serializer,
                    items.trait_name(CompilerItem::SerializeSeq),
                )
                .to_string(),
            struct_serializer_assoc: items
                .trait_assoc_type_by_bound(
                    CompilerItem::Serializer,
                    items.trait_name(CompilerItem::SerializeStruct),
                )
                .to_string(),
            variant_serializer_assoc: items
                .trait_assoc_type_by_bound(
                    CompilerItem::Serializer,
                    items.trait_name(CompilerItem::SerializeVariant),
                )
                .to_string(),
            seq_access_assoc: items
                .trait_assoc_type_by_bound(
                    CompilerItem::Deserializer,
                    items.trait_name(CompilerItem::DeserializeSeq),
                )
                .to_string(),
            struct_access_assoc: items
                .trait_assoc_type_by_bound(
                    CompilerItem::Deserializer,
                    items.trait_name(CompilerItem::DeserializeStruct),
                )
                .to_string(),
            variant_access_assoc: items
                .trait_assoc_type_by_bound(
                    CompilerItem::Deserializer,
                    items.trait_name(CompilerItem::DeserializeVariant),
                )
                .to_string(),
            s_struct_serializer_proj: format!(
                "S::{}",
                items.trait_assoc_type_by_bound(
                    CompilerItem::Serializer,
                    items.trait_name(CompilerItem::SerializeStruct),
                )
            ),
            s_seq_serializer_proj: format!(
                "S::{}",
                items.trait_assoc_type_by_bound(
                    CompilerItem::Serializer,
                    items.trait_name(CompilerItem::SerializeSeq),
                )
            ),
            s_variant_serializer_proj: format!(
                "S::{}",
                items.trait_assoc_type_by_bound(
                    CompilerItem::Serializer,
                    items.trait_name(CompilerItem::SerializeVariant),
                )
            ),
            d_struct_access_proj: format!(
                "D::{}",
                items.trait_assoc_type_by_bound(
                    CompilerItem::Deserializer,
                    items.trait_name(CompilerItem::DeserializeStruct),
                )
            ),
            d_seq_access_proj: format!(
                "D::{}",
                items.trait_assoc_type_by_bound(
                    CompilerItem::Deserializer,
                    items.trait_name(CompilerItem::DeserializeSeq),
                )
            ),
            d_variant_access_proj: format!(
                "D::{}",
                items.trait_assoc_type_by_bound(
                    CompilerItem::Deserializer,
                    items.trait_name(CompilerItem::DeserializeVariant),
                )
            ),
        }
    }
}

use super::common::{
    alloc_local, block, break_stmt, cast, deref_expr, expr_stmt, field_access, i32_const, if_stmt,
    let_mut_stmt, local_ref, loop_stmt, null_expr, option_none, option_some, param_local, ref_expr,
    return_stmt, string_lit, synth_span,
};

/// Wire-form name for a struct field.
///
/// Single source of truth for the serialized key: an explicit
/// `#[serde(rename = "...")]` wins; otherwise `#[serde(rename_all = "...")]`
/// applies its case strategy; otherwise the Wado source field name is used
/// verbatim (identity — `user_id` stays `"user_id"`, `userId` stays
/// `"userId"`). Used by both the serialize and deserialize synthesisers so the
/// two never disagree.
fn serialized_field_name(f: &crate::tir::TirField, struct_def: &crate::tir::TirStruct) -> String {
    f.serde_rename.clone().unwrap_or_else(|| {
        if let Some(strategy) = &struct_def.serde_rename_all {
            apply_rename_all(&f.name, strategy)
        } else {
            f.name.clone()
        }
    })
}

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
        // Unrecognized strategy string: fall back to identity (the name as
        // written), matching the no-attribute default.
        _ => s.to_string(),
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
        let names = {
            let tt = module.type_table.borrow();
            SerdeStdlibNames::from_compiler_items(tt.compiler_items())
        };
        let existing = collect_existing_trait_methods(module);
        let mut generated = Vec::new();

        for req in &requests {
            let trait_name = req.trait_name.as_str();
            if trait_name == names.serialize {
                let key = MethodName::format_local(
                    &req.target_type_name,
                    Some(&names.serialize),
                    "serialize",
                );
                if existing.contains(&key) {
                    continue;
                }
                let func = generate_struct_serialize(module, req, &names)
                    .or_else(|| generate_enum_serialize(module, req, &names))
                    .or_else(|| generate_variant_serialize(module, req, &names))
                    .or_else(|| generate_flags_serialize(module, req, &names));
                if let Some(f) = func {
                    generated.push(Rc::new(RefCell::new(f)));
                }
            } else if trait_name == names.deserialize {
                let key = MethodName::format_local(
                    &req.target_type_name,
                    Some(&names.deserialize),
                    "deserialize",
                );
                if existing.contains(&key) {
                    continue;
                }
                if let Some((lookup_func, deser_func)) =
                    generate_struct_deserialize(module, req, &names)
                {
                    generated.push(Rc::new(RefCell::new(lookup_func)));
                    generated.push(Rc::new(RefCell::new(deser_func)));
                } else {
                    let func = generate_enum_deserialize(module, req, &names)
                        .or_else(|| generate_variant_deserialize(module, req, &names))
                        .or_else(|| generate_flags_deserialize(module, req, &names));
                    if let Some(f) = func {
                        generated.push(Rc::new(RefCell::new(f)));
                    }
                }
            } else {
                panic!(
                    "unsupported synthesis trait `{trait_name}` for `{}`",
                    req.target_type_name
                );
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
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source,
                name: fn_name,
                monomorph_info: None,
                method_info: Some(info),
            },
            type_args,
            args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        ),
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
    names: &SerdeStdlibNames,
) -> TirBlock {
    let extract_err = TirExpr::new(
        TirExprKind::VariantPayload {
            expr: Box::new(local_ref(result_local, result_local_name, result_type)),
            case_index: names.err_index,
            payload_type: err_type,
        },
        err_type,
        span,
    );
    block(vec![return_stmt(Some(variant_err(
        extract_err,
        outer_result_type,
        span,
        names,
    )))])
}

fn variant_ok(
    value: TirExpr,
    result_type: TypeId,
    span: Span,
    names: &SerdeStdlibNames,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantConstruct {
            variant_type: result_type,
            case_index: names.ok_index,
            case_name: names.ok_name.clone(),
            payload: Some(Box::new(value)),
        },
        result_type,
        span,
    )
}

fn variant_err(
    value: TirExpr,
    result_type: TypeId,
    span: Span,
    names: &SerdeStdlibNames,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::VariantConstruct {
            variant_type: result_type,
            case_index: names.err_index,
            case_name: names.err_name.clone(),
            payload: Some(Box::new(value)),
        },
        result_type,
        span,
    )
}

/// Build a TIR Match `match scrutinee { Ok(ok) => then, _ => else }`
/// wrapped in a `TirStmtKind::Expr` so the synthesised statement
/// pipeline can append it like any other side-effecting expression.
///
/// Equivalent to the legacy `TirStmtKind::IfLet` shape; emitted as
/// canonical TIR Match per WEP 2026-05-11 Phase 10 Step 3 so the
/// `lower::translate::pattern` `IfLet` → Match pre-pass has nothing
/// further to rewrite here.
fn if_let_ok(
    scrutinee: TirExpr,
    result_type: TypeId,
    ok_type: TypeId,
    ok_local: u32,
    ok_name: &str,
    then_block: TirBlock,
    else_block: TirBlock,
    span: Span,
    names: &SerdeStdlibNames,
) -> TirStmt {
    variant_match_stmt(
        scrutinee,
        result_type,
        names.ok_name.clone(),
        ok_type,
        ok_local,
        ok_name,
        then_block,
        else_block,
        span,
    )
}

/// Build a TIR Match `match scrutinee { Some(inner) => then, _ => else }`.
/// Same migration shape as [`if_let_ok`].
fn if_let_some(
    scrutinee: TirExpr,
    option_type: TypeId,
    inner_type: TypeId,
    inner_local: u32,
    inner_name: &str,
    then_block: TirBlock,
    else_block: TirBlock,
    span: Span,
    names: &SerdeStdlibNames,
) -> TirStmt {
    variant_match_stmt(
        scrutinee,
        option_type,
        names.some_name.clone(),
        inner_type,
        inner_local,
        inner_name,
        then_block,
        else_block,
        span,
    )
}

/// Shared helper for the variant-arm + wildcard-arm Match used by both
/// `if_let_ok` and `if_let_some`. Wraps each branch's block in a
/// `TirExprKind::Block` so the arm's body expression yields Unit.
fn variant_match_stmt(
    scrutinee: TirExpr,
    enum_type: TypeId,
    case_name: String,
    payload_type: TypeId,
    payload_local: u32,
    payload_name: &str,
    then_block: TirBlock,
    else_block: TirBlock,
    span: Span,
) -> TirStmt {
    let then_span = then_block.span;
    let else_span = else_block.span;
    let then_body = TirExpr::new(TirExprKind::Block(then_block), TypeTable::UNIT, then_span);
    let else_body = TirExpr::new(TirExprKind::Block(else_block), TypeTable::UNIT, else_span);
    let arms = vec![
        TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type,
                variant_name: case_name,
                bindings: vec![TirPattern::Binding {
                    name: payload_name.to_string(),
                    local_index: payload_local,
                    type_id: payload_type,
                }],
                payload_type,
            },
            guard: None,
            body: then_body,
            span: then_span,
        },
        TirMatchArm {
            pattern: TirPattern::Wildcard,
            guard: None,
            body: else_body,
            span: else_span,
        },
    ];
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(scrutinee),
            arms,
        },
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(match_expr), span)
}

fn serialize_error_literal(
    error_type: TypeId,
    error_kind_type: TypeId,
    message: &str,
    string_type: TypeId,
    span: Span,
    names: &SerdeStdlibNames,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: error_type,
            struct_name: names.serialize_error.clone(),
            fields: vec![
                TirStructField {
                    name: "kind".to_string(),
                    value: TirExpr::new(
                        TirExprKind::EnumConstruct {
                            enum_type: error_kind_type,
                            case_index: names.ser_err_custom_index,
                            case_name: names.ser_err_custom_name.clone(),
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
    names: &SerdeStdlibNames,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: error_type,
            struct_name: names.deserialize_error.clone(),
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
/// Only emits the call when the `Default` trait is registered via `#[compiler_item("default")]`.
/// The module source for resolution comes from the type itself (where `impl Default for T` lives).
fn default_value_for_type(type_id: TypeId, type_table: &TypeTable, span: Span) -> TirExpr {
    // Gate: only generate Default calls if the trait is registered via compiler_item.
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
    let default_trait_name = type_table
        .compiler_items()
        .trait_name(crate::compiler_item::CompilerItem::Default)
        .to_string();
    let mut method_info =
        LocalMethodName::new(base_name, Some(default_trait_name), "default".to_string());
    if !type_args.is_empty() {
        let arg_names: Vec<String> = type_args.iter().map(|t| type_table.type_name(*t)).collect();
        method_info = method_info.with_type_args(&arg_names, &[]);
    }
    let mangled_name = method_info.to_mangled_name();
    // For generic types (Option<String>, List<i32>, etc.), set monomorph_info with
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
    names: &SerdeStdlibNames,
) -> Option<TirFunction> {
    let struct_def = find_struct(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let mut tt = module.type_table.borrow_mut();

    let struct_type = req.target_type_id;
    let ref_self_type = tt.make_ref(struct_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct(names.serialize_error.clone(), serde_module.clone());
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);
    let struct_ser_type = tt.make_assoc_type_projection(
        s_type_param,
        names.struct_serializer_assoc.clone(),
        vec![names.serialize_struct.clone()],
        vec![],
    );
    let result_ss_err = tt.make_result(struct_ser_type, ser_error_type);
    let mut_ref_ss = tt.make_mut_ref(struct_ser_type);

    let fields: Vec<(String, String, TypeId, u32)> = struct_def
        .fields
        .iter()
        .map(|f| {
            let serialized_name = serialized_field_name(f, struct_def);
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

    let mut locals = vec![
        param_local("self", ref_self_type, false),
        param_local("s", mut_ref_s, false),
    ];
    let mut next_local: u32 = 2;
    let result_tmp = alloc_local(&mut next_local, &mut locals, result_ss_err);
    let st_local = alloc_local(&mut next_local, &mut locals, struct_ser_type);

    let mut stmts = Vec::new();

    let begin_call = type_param_method_call(
        local_ref(1, "s", mut_ref_s),
        "S",
        &names.serializer,
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
    for (i, (field_name, wire_name, field_type, field_index)) in fields.iter().enumerate() {
        let self_ref = local_ref(0, "self", ref_self_type);
        let self_deref = deref_expr(self_ref, struct_type, span);
        let field_val = field_access(self_deref, *field_index, field_name, *field_type, span);
        let field_ref = ref_expr(field_val, field_ref_types[i], span);

        let field_call = type_param_method_call(
            local_ref(st_local, "st", mut_ref_ss),
            &names.s_struct_serializer_proj,
            &names.serialize_struct,
            "field",
            serde_module.clone(),
            vec![field_type_names[i].clone()],
            vec![*field_type],
            vec![
                ref_expr(
                    string_lit(wire_name, string_type, span),
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
        &names.s_struct_serializer_proj,
        &names.serialize_struct,
        "end",
        serde_module,
        vec![],
        vec![],
        vec![],
        result_unit_err,
        span,
    );
    then_stmts.push(return_stmt(Some(end_call)));

    let else_block = propagate_err_block(
        result_tmp,
        "__result",
        result_ss_err,
        ser_error_type,
        result_unit_err,
        span,
        names,
    );

    stmts.push(if_let_ok(
        local_ref(result_tmp, "__result", result_ss_err),
        result_ss_err,
        struct_ser_type,
        st_local,
        "st",
        block(then_stmts),
        else_block,
        span,
        names,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some(names.serialize.clone()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some(&names.serialize), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.serializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    })
}

fn generate_struct_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
    names: &SerdeStdlibNames,
) -> Option<(TirFunction, TirFunction)> {
    let struct_def = find_struct(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let mut tt = module.type_table.borrow_mut();

    let struct_type = req.target_type_id;
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let option_i32 = tt.make_option(TypeTable::I32);
    let deser_error_type = tt.make_struct(names.deserialize_error.clone(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type(&names.deserialize_error_kind, &serde_module)
        .unwrap_or(TypeTable::I32);
    let result_struct_err = tt.make_result(struct_type, deser_error_type);
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let struct_access_type = tt.make_assoc_type_projection(
        d_type_param,
        names.struct_access_assoc.clone(),
        vec![names.deserialize_struct.clone()],
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
            let serialized_name = serialized_field_name(f, struct_def);
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
    // The struct type spelling drives the `next_field::<StructType>()` type
    // argument that selects this struct's `FieldSchema::lookup` at
    // monomorphization.
    let struct_type_name = tt.type_name(struct_type);

    let compiler_items = tt.compiler_items().clone();
    drop(tt);

    // `impl FieldSchema for <Type> { fn lookup(...) }` — the static, per-type
    // field-name → index matcher. `next_field::<Type>()` resolves it directly
    // at monomorphization, so no closure value is constructed or allocated.
    let lookup_func = generate_lookup_function(
        &req.target_type_name,
        &names.field_schema,
        &fields,
        string_type,
        ref_string_type,
        option_i32,
        span,
        &compiler_items,
    );

    let mut locals = vec![param_local("d", mut_ref_d, false)];
    let mut next_local: u32 = 1;
    let result_tmp = alloc_local(&mut next_local, &mut locals, result_sa_err);
    let sd_local = alloc_local(&mut next_local, &mut locals, struct_access_type);
    let seen_local = alloc_local(&mut next_local, &mut locals, TypeTable::U32);
    let field_locals: Vec<u32> = fields
        .iter()
        .map(|(_, _, type_id, _)| alloc_local(&mut next_local, &mut locals, *type_id))
        .collect();

    let mut stmts = Vec::new();

    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        &names.deserializer,
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
    let next_result_local = alloc_local(&mut next_local, &mut locals, result_opt_i32_err);
    let next_opt_local = alloc_local(&mut next_local, &mut locals, option_i32);
    let field_idx_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);

    let mut loop_stmts = Vec::new();

    let next_field_call = type_param_method_call(
        local_ref(sd_local, "sd", mut_ref_sa),
        &names.d_struct_access_proj,
        &names.deserialize_struct,
        "next_field",
        serde_module.clone(),
        vec![struct_type_name],
        vec![struct_type],
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
            &names.d_struct_access_proj,
            &names.deserialize_struct,
            "value",
            serde_module.clone(),
            vec![field_type_names[i].clone()],
            vec![*type_id],
            vec![],
            field_result_types[i],
            span,
        );
        let val_result_local = alloc_local(&mut next_local, &mut locals, field_result_types[i]);
        let val_ok_local = alloc_local(&mut next_local, &mut locals, *type_id);

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
                    names,
                ),
                span,
                names,
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
        &names.d_struct_access_proj,
        &names.deserialize_struct,
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
        names,
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
            names,
        ),
        span,
        names,
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
                &names.deser_err_missing_field_name,
                names.deser_err_missing_field_index,
                "required field missing",
                string_type,
                span,
                names,
            );
            then_stmts.push(if_stmt(
                ne_check,
                block(vec![return_stmt(Some(variant_err(
                    missing_err,
                    result_struct_err,
                    span,
                    names,
                )))]),
                None,
            ));
        }
    }

    // sd.end()
    let end_call = type_param_method_call(
        local_ref(sd_local, "sd", mut_ref_sa),
        &names.d_struct_access_proj,
        &names.deserialize_struct,
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
        names,
    ))));

    let else_block = propagate_err_block(
        result_tmp,
        "__result",
        result_sa_err,
        deser_error_type,
        result_struct_err,
        span,
        names,
    );

    stmts.push(if_let_ok(
        local_ref(result_tmp, "__result", result_sa_err),
        result_sa_err,
        struct_access_type,
        sd_local,
        "sd",
        block(then_stmts),
        else_block,
        span,
        names,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some(names.deserialize.clone()),
        "deserialize".to_string(),
    );
    let qualified_name = MethodName::format_local(
        &req.target_type_name,
        Some(&names.deserialize),
        "deserialize",
    );

    let deser_func = TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.deserializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    };

    Some((lookup_func, deser_func))
}

/// Build a `string.get_byte_unchecked(index_expr) as i32` expression with a computed index.
///
/// The method is looked up via [`CompilerItem::StringGetByteUnchecked`] —
/// renaming the underlying Wado declaration cannot break this site as
/// long as its `#[compiler_item("string_get_byte_unchecked")]` annotation
/// stays put. See issue #1077 for the rationale.
fn key_get_byte_as_i32_expr(
    string_ref: TirExpr,
    index_expr: TirExpr,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirExpr {
    let (module_source, owner, name) =
        compiler_items.require_method(crate::compiler_item::CompilerItem::StringGetByteUnchecked);
    let get_byte_method = LocalMethodName::new(owner.to_string(), None, name.to_string());
    let get_byte_call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(string_ref),
            FunctionRef {
                module_source: module_source.clone(),
                name: get_byte_method.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(get_byte_method),
            },
            vec![],
            vec![CallArg::new(index_expr, false)],
        ),
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
    field_schema_trait: &str,
    fields: &[(String, String, TypeId, u32)],
    _string_type: TypeId,
    ref_string_type: TypeId,
    option_i32: TypeId,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirFunction {
    // `impl FieldSchema for <Type> { fn lookup(input, start, end) }` — a static
    // trait method (no `self`). `next_field::<Type>()` resolves it directly at
    // monomorphization, replacing the former runtime `lookup` closure.
    let fn_name = MethodName::format_local(type_name, Some(field_schema_trait), "lookup");
    // Parameters: input: &String (0), start: i32 (1), end: i32 (2)
    let mut locals = vec![
        param_local("__input", ref_string_type, false),
        param_local("__start", TypeTable::I32, false),
        param_local("__end", TypeTable::I32, false),
    ];
    let mut next_local: u32 = 3;

    let mut stmts = Vec::new();

    // Allocate a local for `let __len = end - start`
    let len_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
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
    for (i, (_, wire_name, _, _)) in fields.iter().enumerate() {
        let name_bytes = wire_name.as_bytes();
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
                    compiler_items,
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
                compiler_items,
            )))]),
            None,
        ));
    }
    stmts.push(return_stmt(Some(option_none(option_i32, compiler_items))));

    TirFunction {
        module_source: ModuleSource::default(),
        name: fn_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(LocalMethodName::new(
            type_name.to_string(),
            Some(field_schema_trait.to_string()),
            "lookup".to_string(),
        )),
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
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
    names: &SerdeStdlibNames,
) -> Option<TirFunction> {
    let enum_def = find_enum(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let mut tt = module.type_table.borrow_mut();

    let enum_type = req.target_type_id;
    let ref_self_type = tt.make_ref(enum_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct(names.serialize_error.clone(), serde_module.clone());
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);

    let cases: Vec<(String, u32)> = enum_def
        .cases
        .iter()
        .map(|c| (c.name.clone(), c.index))
        .collect();

    drop(tt);

    let locals = vec![
        param_local("self", ref_self_type, false),
        param_local("s", mut_ref_s, false),
    ];
    let next_local: u32 = 2;

    // Build match arms: one per enum case calling serialize_unit_variant
    let mut match_arms = Vec::new();
    for (case_name, case_index) in &cases {
        let call = type_param_method_call(
            local_ref(1, "s", mut_ref_s),
            "S",
            &names.serializer,
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
        Some(names.serialize.clone()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some(&names.serialize), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.serializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    })
}

fn generate_enum_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
    names: &SerdeStdlibNames,
) -> Option<TirFunction> {
    let enum_def = find_enum(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let (eq_trait_name, string_struct_name) = {
        let tt = module.type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .trait_name(crate::compiler_item::CompilerItem::Eq)
                .to_string(),
            items
                .struct_name(crate::compiler_item::CompilerItem::String)
                .to_string(),
        )
    };
    let mut tt = module.type_table.borrow_mut();

    let enum_type = req.target_type_id;
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let deser_error_type = tt.make_struct(names.deserialize_error.clone(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type(&names.deserialize_error_kind, &serde_module)
        .unwrap_or(TypeTable::I32);
    let result_enum_err = tt.make_result(enum_type, deser_error_type);
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let variant_access_type = tt.make_assoc_type_projection(
        d_type_param,
        names.variant_access_assoc.clone(),
        vec![names.deserialize_variant.clone()],
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

    let mut locals = vec![param_local("d", mut_ref_d, false)];
    let mut next_local: u32 = 1;
    let va_result_local = alloc_local(&mut next_local, &mut locals, result_va_err);
    let va_local = alloc_local(&mut next_local, &mut locals, variant_access_type);
    let disc_result_local = alloc_local(&mut next_local, &mut locals, result_i32_err);
    let disc_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
    let name_result_local = alloc_local(&mut next_local, &mut locals, result_string_err);
    let name_local = alloc_local(&mut next_local, &mut locals, string_type);

    let mut stmts = Vec::new();

    // let __va_r = d.begin_variant(&"TypeName", num_cases)
    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        &names.deserializer,
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
        &names.d_variant_access_proj,
        &names.deserialize_variant,
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
            &names.d_variant_access_proj,
            &names.deserialize_variant,
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
            return_stmt(Some(variant_ok(
                enum_construct,
                result_enum_err,
                span,
                names,
            ))),
        ]);
        disc_then_stmts.push(if_stmt(condition, if_body, None));
    }

    // Unknown disc error
    let disc_unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        &names.deser_err_unknown_variant_name,
        names.deser_err_unknown_variant_index,
        "unknown variant discriminant",
        string_type,
        span,
        names,
    );
    disc_then_stmts.push(return_stmt(Some(variant_err(
        disc_unknown_err,
        result_enum_err,
        span,
        names,
    ))));

    // if let Ok(__disc) = __disc_r { disc_matching } else { name fallback }

    // --- name-based fallback (existing logic) ---
    let mut name_fallback_stmts = Vec::new();

    // let __name_r = va.variant_name()
    let name_call = type_param_method_call(
        local_ref(va_local, "va", mut_ref_va),
        &names.d_variant_access_proj,
        &names.deserialize_variant,
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
            string_struct_name.clone(),
            Some(eq_trait_name.clone()),
            "eq".to_string(),
        );
        let condition = TirExpr::new(
            TirExprKind::method_call(
                Box::new(key_ref),
                FunctionRef {
                    module_source: ModuleSource::string(),
                    name: eq_method.to_mangled_name(),
                    monomorph_info: None,
                    method_info: Some(eq_method),
                },
                vec![],
                vec![CallArg::new(lit_ref, false)],
            ),
            TypeTable::BOOL,
            span,
        );

        let end_call = type_param_method_call(
            local_ref(va_local, "va", mut_ref_va),
            &names.d_variant_access_proj,
            &names.deserialize_variant,
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
            return_stmt(Some(variant_ok(
                enum_construct,
                result_enum_err,
                span,
                names,
            ))),
        ]);
        name_then_stmts.push(if_stmt(condition, if_body, None));
    }

    // Unknown variant error
    let unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        &names.deser_err_unknown_variant_name,
        names.deser_err_unknown_variant_index,
        "unknown variant",
        string_type,
        span,
        names,
    );
    name_then_stmts.push(return_stmt(Some(variant_err(
        unknown_err,
        result_enum_err,
        span,
        names,
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
            names,
        ),
        span,
        names,
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
        names,
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
            names,
        ),
        span,
        names,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some(names.deserialize.clone()),
        "deserialize".to_string(),
    );
    let qualified_name = MethodName::format_local(
        &req.target_type_name,
        Some(&names.deserialize),
        "deserialize",
    );

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.deserializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    })
}

fn generate_variant_serialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
    names: &SerdeStdlibNames,
) -> Option<TirFunction> {
    let variant_def = find_variant(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let mut tt = module.type_table.borrow_mut();

    let variant_type = req.target_type_id;
    let ref_self_type = tt.make_ref(variant_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct(names.serialize_error.clone(), serde_module.clone());
    let ser_error_kind_type = tt
        .find_enum_type(&names.serialize_error_kind, &serde_module)
        .unwrap_or(TypeTable::I32);
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);
    let variant_ser_type = tt.make_assoc_type_projection(
        s_type_param,
        names.variant_serializer_assoc.clone(),
        vec![names.serialize_variant.clone()],
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

    let mut locals = vec![
        param_local("self", ref_self_type, false),
        param_local("s", mut_ref_s, false),
    ];
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
                &names.serializer,
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
            let payload_local = alloc_local(&mut next_local, &mut locals, *payload_type);
            let vs_result_local = alloc_local(&mut next_local, &mut locals, result_vs_err);
            let vs_local = alloc_local(&mut next_local, &mut locals, variant_ser_type);

            let begin_call = type_param_method_call(
                local_ref(1, "s", mut_ref_s),
                "S",
                &names.serializer,
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
                &names.s_variant_serializer_proj,
                &names.serialize_variant,
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
                &names.s_variant_serializer_proj,
                &names.serialize_variant,
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
                names,
            );

            let then_block = block(vec![expr_stmt(payload_call), return_stmt(Some(end_call))]);
            let else_block = block(vec![return_stmt(Some(variant_err(
                err_val,
                result_unit_err,
                span,
                names,
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
                    names,
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
        Some(names.serialize.clone()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some(&names.serialize), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.serializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    })
}

fn generate_variant_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
    names: &SerdeStdlibNames,
) -> Option<TirFunction> {
    let variant_def = find_variant(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let (eq_trait_name, string_struct_name) = {
        let tt = module.type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .trait_name(crate::compiler_item::CompilerItem::Eq)
                .to_string(),
            items
                .struct_name(crate::compiler_item::CompilerItem::String)
                .to_string(),
        )
    };
    let mut tt = module.type_table.borrow_mut();

    let variant_type = req.target_type_id;
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let deser_error_type = tt.make_struct(names.deserialize_error.clone(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type(&names.deserialize_error_kind, &serde_module)
        .unwrap_or(TypeTable::I32);
    let result_variant_err = tt.make_result(variant_type, deser_error_type);
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let variant_access_type = tt.make_assoc_type_projection(
        d_type_param,
        names.variant_access_assoc.clone(),
        vec![names.deserialize_variant.clone()],
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

    let mut locals = vec![param_local("d", mut_ref_d, false)];
    let mut next_local: u32 = 1;
    let va_result_local = alloc_local(&mut next_local, &mut locals, result_va_err);
    let va_local = alloc_local(&mut next_local, &mut locals, variant_access_type);
    let disc_result_local = alloc_local(&mut next_local, &mut locals, result_i32_err);
    let disc_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
    let name_result_local = alloc_local(&mut next_local, &mut locals, result_string_err);
    let name_local = alloc_local(&mut next_local, &mut locals, string_type);

    let mut stmts = Vec::new();

    // let __va_r = d.begin_variant(&"TypeName", num_cases)
    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        &names.deserializer,
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
        &names.d_variant_access_proj,
        &names.deserialize_variant,
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
                &names.d_variant_access_proj,
                &names.deserialize_variant,
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
                return_stmt(Some(variant_ok(construct, result_variant_err, span, names))),
            ]);
            disc_then_stmts.push(if_stmt(condition, if_body, None));
        } else {
            let payload_local = alloc_local(&mut next_local, &mut locals, *payload_type);
            let p_result_local = alloc_local(&mut next_local, &mut locals, payload_result_types[i]);

            let payload_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                &names.d_variant_access_proj,
                &names.deserialize_variant,
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
                &names.d_variant_access_proj,
                &names.deserialize_variant,
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
                return_stmt(Some(variant_ok(construct, result_variant_err, span, names))),
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
                        names,
                    ),
                    span,
                    names,
                ),
            ]);
            disc_then_stmts.push(if_stmt(condition, if_body, None));
        }
    }

    let disc_unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        &names.deser_err_unknown_variant_name,
        names.deser_err_unknown_variant_index,
        "unknown variant discriminant",
        string_type,
        span,
        names,
    );
    disc_then_stmts.push(return_stmt(Some(variant_err(
        disc_unknown_err,
        result_variant_err,
        span,
        names,
    ))));

    // --- name-based fallback ---
    let mut name_fallback_stmts = Vec::new();

    let name_call = type_param_method_call(
        local_ref(va_local, "va", mut_ref_va),
        &names.d_variant_access_proj,
        &names.deserialize_variant,
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
            string_struct_name.clone(),
            Some(eq_trait_name.clone()),
            "eq".to_string(),
        );
        let condition = TirExpr::new(
            TirExprKind::method_call(
                Box::new(key_ref),
                FunctionRef {
                    module_source: ModuleSource::string(),
                    name: eq_method.to_mangled_name(),
                    monomorph_info: None,
                    method_info: Some(eq_method),
                },
                vec![],
                vec![CallArg::new(lit_ref, false)],
            ),
            TypeTable::BOOL,
            span,
        );

        if is_unit {
            let end_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                &names.d_variant_access_proj,
                &names.deserialize_variant,
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
                return_stmt(Some(variant_ok(construct, result_variant_err, span, names))),
            ]);
            name_then_stmts.push(if_stmt(condition, if_body, None));
        } else {
            let payload_local = alloc_local(&mut next_local, &mut locals, *payload_type);
            let p_result_local = alloc_local(&mut next_local, &mut locals, payload_result_types[i]);

            let payload_call = type_param_method_call(
                local_ref(va_local, "va", mut_ref_va),
                &names.d_variant_access_proj,
                &names.deserialize_variant,
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
                &names.d_variant_access_proj,
                &names.deserialize_variant,
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
                return_stmt(Some(variant_ok(construct, result_variant_err, span, names))),
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
                        names,
                    ),
                    span,
                    names,
                ),
            ]);
            name_then_stmts.push(if_stmt(condition, if_body, None));
        }
    }

    // Unknown variant error
    let unknown_err = deserialize_error_literal(
        deser_error_type,
        deser_error_kind_type,
        &names.deser_err_unknown_variant_name,
        names.deser_err_unknown_variant_index,
        "unknown variant",
        string_type,
        span,
        names,
    );
    name_then_stmts.push(return_stmt(Some(variant_err(
        unknown_err,
        result_variant_err,
        span,
        names,
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
            names,
        ),
        span,
        names,
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
        names,
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
            names,
        ),
        span,
        names,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some(names.deserialize.clone()),
        "deserialize".to_string(),
    );
    let qualified_name = MethodName::format_local(
        &req.target_type_name,
        Some(&names.deserialize),
        "deserialize",
    );

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.deserializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
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
    names: &SerdeStdlibNames,
) -> Option<TirFunction> {
    let flags_def = find_flags(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let string_struct_name = module
        .type_table
        .borrow()
        .compiler_items()
        .struct_name(crate::compiler_item::CompilerItem::String)
        .to_string();
    let mut tt = module.type_table.borrow_mut();

    let flags_type = req.target_type_id;
    let ref_self_type = tt.make_ref(flags_type);
    let s_type_param = tt.make_type_param("S".to_string(), 0);
    let mut_ref_s = tt.make_mut_ref(s_type_param);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let ser_error_type = tt.make_struct(names.serialize_error.clone(), serde_module.clone());
    let ser_error_kind_type = tt
        .find_enum_type(&names.serialize_error_kind, &serde_module)
        .unwrap_or(TypeTable::I32);
    let result_unit_err = tt.make_result(TypeTable::UNIT, ser_error_type);
    let seq_ser_type = tt.make_assoc_type_projection(
        s_type_param,
        names.seq_serializer_assoc.clone(),
        vec![names.serialize_seq.clone()],
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

    let mut locals = vec![
        param_local("self", ref_self_type, false),
        param_local("s", mut_ref_s, false),
    ];
    let mut next_local: u32 = 2;
    let count_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
    let result_tmp = alloc_local(&mut next_local, &mut locals, result_seq_err);
    let seq_local = alloc_local(&mut next_local, &mut locals, seq_ser_type);

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
        &names.serializer,
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
            &names.s_seq_serializer_proj,
            &names.serialize_seq,
            "element",
            serde_module.clone(),
            vec![string_struct_name.clone()],
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
        &names.s_seq_serializer_proj,
        &names.serialize_seq,
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
        names,
    );
    let else_stmts = vec![return_stmt(Some(variant_err(
        err_val,
        result_unit_err,
        span,
        names,
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
        names,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some(names.serialize.clone()),
        "serialize".to_string(),
    );
    let qualified_name =
        MethodName::format_local(&req.target_type_name, Some(&names.serialize), "serialize");

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "S".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.serializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    })
}

/// Generate `impl Deserialize for FlagsType` — deserializes from array of flag name strings.
fn generate_flags_deserialize(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
    names: &SerdeStdlibNames,
) -> Option<TirFunction> {
    let flags_def = find_flags(module, &req.target_type_name)?;
    let span = synth_span();
    let serde_module = ModuleSource::serde();

    let (eq_trait_name, string_struct_name) = {
        let tt = module.type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .trait_name(crate::compiler_item::CompilerItem::Eq)
                .to_string(),
            items
                .struct_name(crate::compiler_item::CompilerItem::String)
                .to_string(),
        )
    };
    let mut tt = module.type_table.borrow_mut();

    let flags_type = req.target_type_id;
    let d_type_param = tt.make_type_param("D".to_string(), 0);
    let mut_ref_d = tt.make_mut_ref(d_type_param);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let deser_error_type = tt.make_struct(names.deserialize_error.clone(), serde_module.clone());
    let deser_error_kind_type = tt
        .find_enum_type(&names.deserialize_error_kind, &serde_module)
        .unwrap_or(TypeTable::I32);
    let result_flags_err = tt.make_result(flags_type, deser_error_type);
    let result_unit_err = tt.make_result(TypeTable::UNIT, deser_error_type);
    let seq_access_type = tt.make_assoc_type_projection(
        d_type_param,
        names.seq_access_assoc.clone(),
        vec![names.deserialize_seq.clone()],
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

    let mut locals = vec![param_local("d", mut_ref_d, false)];
    let mut next_local: u32 = 1;
    let result_seq_local = alloc_local(&mut next_local, &mut locals, result_seq_err);
    let seq_local = alloc_local(&mut next_local, &mut locals, seq_access_type);
    let bits_local = alloc_local(&mut next_local, &mut locals, TypeTable::U32);
    let elem_result_local = alloc_local(&mut next_local, &mut locals, result_option_string_err);
    let elem_local = alloc_local(&mut next_local, &mut locals, option_string);
    let name_local = alloc_local(&mut next_local, &mut locals, string_type);

    let mut stmts = Vec::new();

    // let __seq_r = d.begin_seq();
    let begin_call = type_param_method_call(
        local_ref(0, "d", mut_ref_d),
        "D",
        &names.deserializer,
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
        &names.d_seq_access_proj,
        &names.deserialize_seq,
        "next_element",
        serde_module.clone(),
        vec![string_struct_name.clone()],
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
            string_struct_name.clone(),
            Some(eq_trait_name.clone()),
            "eq".to_string(),
        );
        let condition = TirExpr::new(
            TirExprKind::method_call(
                Box::new(key_ref),
                FunctionRef {
                    module_source: ModuleSource::string(),
                    name: eq_method.to_mangled_name(),
                    monomorph_info: None,
                    method_info: Some(eq_method),
                },
                vec![],
                vec![CallArg::new(lit_ref, false)],
            ),
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
        &names.deser_err_invalid_value_name,
        names.deser_err_invalid_value_index,
        unknown_msg,
        span,
        names,
    );
    name_match_stmts.push(return_stmt(Some(variant_err(
        unknown_err,
        result_flags_err,
        span,
        names,
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
        names,
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
        names,
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
        names,
    ));

    ok_stmts.push(loop_stmt(block(loop_body_stmts)));

    // seq.end()?
    let end_call = type_param_method_call(
        local_ref(seq_local, "seq", mut_ref_seq),
        &names.d_seq_access_proj,
        &names.deserialize_seq,
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
        names,
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
            names,
        ),
        span,
        names,
    ));

    let method_info = LocalMethodName::new(
        req.target_type_name.clone(),
        Some(names.deserialize.clone()),
        "deserialize".to_string(),
    );
    let qualified_name = MethodName::format_local(
        &req.target_type_name,
        Some(&names.deserialize),
        "deserialize",
    );

    Some(TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        is_pub: true,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        is_async: false,
        type_params: vec![TirTypeParam {
            name: "D".to_string(),
            is_effect: false,
            is_pack: false,
            bounds: vec![names.deserializer.clone()],
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
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(stmts)),
        span,
        local_count: next_local,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
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
    names: &SerdeStdlibNames,
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
            struct_name: names.deserialize_error.clone(),
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
