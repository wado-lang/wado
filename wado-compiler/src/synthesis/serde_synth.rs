//! Serde synthesis phase.
//!
//! Generates struct `Deserialize`, and the `FieldSchema` lookup it reads, for
//! types with a synthesis request. Every other body is derived in Wado, by the
//! `Reflect*` blankets in `core:serde` (WEP 2026-06-13); this pass still drains
//! their requests, which is what makes the bound demand the reflection impls.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::{CompilerItem, CompilerItems};
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, mangle_local_trait_method};
use crate::package::Package;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, ResolvedType, SynthTrait, SynthesisRequest,
    TirBinaryOp, TirExpr, TirExprKind, TirFunction, TirLocal, TirModule, TirParam, TirStmt, TypeId,
    TypeTable,
};
use crate::token::Span;

/// Snapshot of the `core:serde` symbol names this synthesiser needs, resolved
/// once through the `CompilerItem` registry. Mirrors
/// [`super::cm_binding::types::CmStdlibNames`]: built per-call, cheap (a
/// registry hit plus a clone), and threaded through the synthesis helpers so a
/// stdlib rename flows through every call site without touching Rust code.
///
/// Field set is **minimal**: with the deserialize body derived in Wado, the
/// only symbol left is the trait the synthesized `lookup` / `positional_at`
/// implement.
#[derive(Clone, Debug)]
pub(super) struct SerdeStdlibNames {
    pub deserialize: String,
    pub field_schema: String,
}

impl SerdeStdlibNames {
    pub fn from_compiler_items(items: &CompilerItems) -> Self {
        Self {
            deserialize: items.trait_name(CompilerItem::Deserialize).to_string(),
            field_schema: items.trait_name(CompilerItem::FieldSchema).to_string(),
        }
    }
}

use super::common::{
    alloc_local, block, i32_const, if_stmt, let_mut_stmt, local_ref, option_none, option_some,
    param_local, return_stmt, synth_span,
};

/// Wire-form name for a struct field.
///
/// Single source of truth for the serialized key: an explicit
/// `#[wire(name = "...")]` wins; otherwise `#[wire(name_policy = "...")]`
/// applies its case strategy; otherwise the Wado source field name is used
/// verbatim (identity — `user_id` stays `"user_id"`, `userId` stays
/// `"userId"`). Used by both the serialize and deserialize synthesisers so the
/// two never disagree.
fn serialized_field_name(f: &crate::tir::TirField, struct_def: &crate::tir::TirStruct) -> String {
    f.wire_name_override.clone().unwrap_or_else(|| {
        if let Some(strategy) = &struct_def.wire_name_policy {
            apply_name_policy(&f.name, strategy)
        } else {
            f.name.clone()
        }
    })
}

/// Apply a `name_policy` strategy. Source-casing-agnostic, so it works for both
/// `snake_case` struct fields and `PascalCase` enum/variant cases (Wado casing is
/// convention, not a rule, so the source form is open).
fn apply_name_policy(s: &str, strategy: &str) -> String {
    use heck::{
        ToKebabCase, ToLowerCamelCase, ToShoutyKebabCase, ToShoutySnakeCase, ToSnakeCase,
        ToUpperCamelCase,
    };
    match strategy {
        "camelCase" => s.to_lower_camel_case(),
        "snake_case" => s.to_snake_case(),
        "PascalCase" => s.to_upper_camel_case(),
        "SCREAMING_SNAKE_CASE" => s.to_shouty_snake_case(),
        "kebab-case" => s.to_kebab_case(),
        "SCREAMING-KEBAB-CASE" => s.to_shouty_kebab_case(),
        // Unrecognized strategy string: fall back to identity (the name as
        // written), matching the no-attribute default.
        _ => s.to_string(),
    }
}

pub fn synthesize_serde(project: &mut Package) {
    distribute_bound_driven_requests(project);

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
            match req.trait_ref {
                // `Serialize` is derived in Wado, by the blanket impls over
                // `ReflectStruct` / `ReflectVariant` / `ReflectEnum` /
                // `ReflectFlags` in `core:serde`. The request still arrives —
                // it is what makes the bound demand the reflection impls — and
                // there is nothing here left to generate for it.
                SynthTrait::Serialize => {}
                SynthTrait::Deserialize => {
                    let key = MethodName::format_local(
                        &FqTypeName::declared(&module.module_source, &req.target_type_name),
                        Some(&names.deserialize),
                        "deserialize",
                    );
                    if existing.contains(&key) {
                        continue;
                    }
                    if let Some((lookup_func, positional_at_func)) =
                        generate_field_schema(module, req, &names)
                    {
                        generated.push(Rc::new(RefCell::new(lookup_func)));
                        generated.push(Rc::new(RefCell::new(positional_at_func)));
                    }
                }
                // `From` requests are drained by `from_synth`, which runs
                // before this pass, so none reach the serde drain.
                SynthTrait::From { .. } => unreachable!(
                    "From synthesis requests must be drained by from_synth before serde_synth"
                ),
            }
        }

        module.functions.extend(generated);
    }
}

/// Distribute bound-driven `Serialize` / `Deserialize` requests (WEP
/// 2026-06-25-trait-derivation) into the `synthesis_requests` of each
/// request's own defining module — exactly where an explicit
/// `impl Trait for T;` marker's request already lives — so the drain loop
/// above sees one uniform list regardless of which path produced an entry.
///
/// Reads `TypeTable::bound_driven_synth_requests` as a snapshot, not a
/// drain: `synthesis::traits::synthesize_traits` reads the same shared set
/// for its `Eq` / `Ord` entries, so this pass only consumes the `Serialize`
/// / `Deserialize` ones (matched by trait name) and leaves the rest.
fn distribute_bound_driven_requests(project: &mut Package) {
    let Some(type_table) = project
        .tir_modules
        .values()
        .next()
        .map(|m| m.type_table.clone())
    else {
        return;
    };

    let (serialize_name, deserialize_name) = {
        let tt = type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .trait_name_opt(CompilerItem::Serialize)
                .map(str::to_string),
            items
                .trait_name_opt(CompilerItem::Deserialize)
                .map(str::to_string),
        )
    };

    // Fetch and filter to just ours (Eq/Ord entries belong to
    // `synthesize_traits`) before the scan below, so a program with no
    // bound-driven serde requests skips it entirely.
    let requests = type_table
        .borrow()
        .bound_driven_synth_requests(|trait_name| {
            Some(trait_name) == serialize_name.as_deref()
                || Some(trait_name) == deserialize_name.as_deref()
        });
    if requests.is_empty() {
        return;
    }

    // One pass to resolve every declared struct/enum/variant/flags name to
    // its `TypeId` (`SynthesisRequest` needs one), instead of an O(requests
    // × types) rescan per entry.
    let by_name: IndexMap<(String, ModuleSource), TypeId> = {
        let tt = type_table.borrow();
        tt.all_types()
            .filter_map(|(id, resolved)| match resolved {
                ResolvedType::Struct {
                    decl_name: name,
                    module_source,
                    ..
                }
                | ResolvedType::Enum {
                    name,
                    module_source,
                }
                | ResolvedType::Variant {
                    name,
                    module_source,
                }
                | ResolvedType::Flags {
                    name,
                    module_source,
                } => Some(((name.clone(), module_source.clone()), id)),
                _ => None,
            })
            .collect()
    };

    for (target_type_name, module_source, trait_name) in requests {
        // `requests` is already filtered to serialize_name/deserialize_name,
        // so the else branch below is always Deserialize.
        let trait_ref = if Some(trait_name.as_str()) == serialize_name.as_deref() {
            SynthTrait::Serialize
        } else {
            SynthTrait::Deserialize
        };
        let Some(&target_type_id) = by_name.get(&(target_type_name.clone(), module_source.clone()))
        else {
            continue;
        };
        let Some(module) = project.tir_modules.get_mut(&module_source) else {
            continue;
        };
        module.synthesis_requests.push(SynthesisRequest {
            trait_ref,
            target_type_name,
            target_type_id,
            type_params: Vec::new(),
            span: Span::default(),
        });
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
                    mangle_local_trait_method(
                        &info.base_struct_name(),
                        trait_name,
                        &info.method_name,
                    )
                })
            })
        })
        .collect()
}

fn find_struct<'a>(module: &'a TirModule, name: &str) -> Option<&'a crate::tir::TirStruct> {
    module.structs.iter().find(|s| s.name == name)
}

/// Generate the `FieldSchema` impl a struct's `Deserialize` derivation reads:
/// `lookup` maps a wire key's bytes to a field index, `positional_at` maps an
/// ordinal rank to one. The deserialize body itself is derived in Wado, by the
/// `ReflectStruct` blanket in `core:serde` (WEP 2026-06-13).
fn generate_field_schema(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
    names: &SerdeStdlibNames,
) -> Option<(TirFunction, TirFunction)> {
    let struct_def = find_struct(module, &req.target_type_name)?;
    let span = synth_span();

    let mut tt = module.type_table.borrow_mut();
    let option_i32 = tt.make_option(TypeTable::I32);
    // `FieldSchema::lookup(key: ByteSlice)`: the slice module comes from the
    // byte-read compiler item, wrapped in the `ByteSlice` newtype to match the
    // trait signature.
    let key_slice_type = {
        let slice_module = tt
            .compiler_method(crate::compiler_item::CompilerItem::ByteSliceGetUnchecked)
            .0
            .clone();
        let base =
            tt.make_generic_instance("ArraySlice".to_string(), slice_module, vec![TypeTable::U8]);
        tt.make_newtype(
            "ByteSlice".to_string(),
            crate::module_source::ModuleSource::bytes(),
            base,
        )
    };
    let fields: Vec<(String, String, TypeId, u32)> = struct_def
        .fields
        .iter()
        .map(|f| {
            let serialized_name = serialized_field_name(f, struct_def);
            (f.name.clone(), serialized_name, f.type_id, f.index)
        })
        .collect();
    // `next_field::<Type>()` resolves `FieldSchema::lookup` by substituting the
    // concrete receiver, so the definition must carry the same fq receiver the
    // substitution builds.
    let target_fq = tt.fq_base_type_name(req.target_type_id);
    let compiler_items = tt.compiler_items().clone();
    drop(tt);

    // `#[wire(positional)]` flags, aligned with `fields` (enumerate index ==
    // the field index the derivation writes). A positional field is ordinal:
    // `lookup` omits it, `positional_at` enumerates it.
    let positional_flags: Vec<bool> = struct_def
        .fields
        .iter()
        .map(|f| f.serde_positional)
        .collect();

    let mut lookup_func = generate_lookup_function(
        &target_fq,
        &names.field_schema,
        &fields,
        &positional_flags,
        key_slice_type,
        option_i32,
        span,
        &compiler_items,
    );
    let mut positional_at_func = generate_positional_at_function(
        &target_fq,
        &names.field_schema,
        &positional_flags,
        option_i32,
        span,
        &compiler_items,
    );
    // A generic struct's schema is one impl over `S<T, …>`, like its reflect
    // impls: the derivation calls `next_field::<T>()` with the instance, so the
    // methods must instantiate alongside it. Neither body reads the parameters.
    lookup_func
        .impl_type_params
        .clone_from(&struct_def.type_params);
    positional_at_func
        .impl_type_params
        .clone_from(&struct_def.type_params);
    Some((lookup_func, positional_at_func))
}

/// Build a `key.get_unchecked(index_expr) as i32` expression on a
/// `ByteSlice` (`ArraySlice<u8>`) key, with a computed index.
///
/// The method is looked up via [`CompilerItem::ByteSliceGetUnchecked`] —
/// renaming the underlying Wado declaration cannot break this site as
/// long as its `#[compiler_item("byte_slice_get_unchecked")]` annotation
/// stays put. See issue #1077 for the rationale.
fn key_get_byte_as_i32_expr(
    key_ref: TirExpr,
    index_expr: TirExpr,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirExpr {
    let get_byte_call = byte_slice_method_call(
        crate::compiler_item::CompilerItem::ByteSliceGetUnchecked,
        key_ref,
        vec![CallArg::new(index_expr, false)],
        TypeTable::U8,
        span,
        compiler_items,
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

/// Build a `key.len()` expression on a `ByteSlice` (`ArraySlice<u8>`) key,
/// returning the byte length as `i32`.
fn byte_slice_len_expr(
    key_ref: TirExpr,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirExpr {
    byte_slice_method_call(
        crate::compiler_item::CompilerItem::ByteSliceLen,
        key_ref,
        vec![],
        TypeTable::I32,
        span,
        compiler_items,
    )
}

/// Build a call to a generic `ArraySlice<T>` method on a `ByteSlice`
/// (`ArraySlice<u8>`) receiver, monomorphized at `T = u8`.
///
/// The method is resolved via `#[compiler_item]` (rename-safe, per issue
/// #1077) and called with `impl_type_args = [u8]` so the monomorphizer
/// instantiates the `u8` specialization instead of inheriting the enclosing
/// function's type parameters — the same shape as the generic `default()`
/// call synthesised for field defaults.
fn byte_slice_method_call(
    item: crate::compiler_item::CompilerItem,
    receiver: TirExpr,
    args: Vec<CallArg>,
    return_type: TypeId,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirExpr {
    let (module_source, owner, name) = compiler_items.require_method(item);
    let method_info = LocalMethodName::new(
        FqTypeName::declared(module_source, owner),
        None,
        name.to_string(),
    )
    .with_struct_type_args(&[FqTypeName::builtin("u8")]);
    let monomorph_info = crate::tir::MonomorphInfo {
        generic_name: method_info.base_struct_name(),
        impl_type_args: vec![TypeTable::U8],
        method_type_args: vec![],
        is_blanket: false,
    };
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: module_source.clone(),
                name: method_info.to_mangled_name(),
                monomorph_info: Some(monomorph_info),
                method_info: Some(method_info),
            },
            vec![],
            args,
        ),
        return_type,
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

/// Assemble a static `FieldSchema` method (`lookup` / `positional_at`): a
/// no-`self`, single-`i32`-or-bytes-param function returning `Option<i32>`.
/// Both methods share this boilerplate; only the name, parameter, and body
/// differ.
#[allow(clippy::too_many_arguments)]
fn field_schema_method_fn(
    type_name: &FqTypeName,
    field_schema_trait: &str,
    method: &str,
    param_name: &str,
    param_type: TypeId,
    return_type: TypeId,
    locals: Vec<TirLocal>,
    local_count: u32,
    body: Vec<TirStmt>,
    span: Span,
) -> TirFunction {
    TirFunction {
        module_source: ModuleSource::default(),
        name: MethodName::format_local(type_name, Some(field_schema_trait), method),
        visibility: crate::ast::Visibility::Public,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        benign_effects: Vec::new(),
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(LocalMethodName::new(
            type_name.clone(),
            Some(field_schema_trait.to_string()),
            method.to_string(),
        )),
        params: vec![TirParam {
            name: param_name.to_string(),
            type_id: param_type,
            local_index: 0,
            is_mut: false,
            is_mut_ref: false,
            span,
        }],
        return_type,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(body)),
        span,
        local_count,
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

fn generate_lookup_function(
    type_name: &FqTypeName,
    field_schema_trait: &str,
    fields: &[(String, String, TypeId, u32)],
    positional_flags: &[bool],
    key_slice_type: TypeId,
    option_i32: TypeId,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirFunction {
    // `impl FieldSchema for <Type> { fn lookup(key: ByteSlice) }` — a static
    // trait method (no `self`). `next_field::<Type>()` resolves it directly at
    // monomorphization, replacing the former runtime `lookup` closure. The key
    // is a borrowed byte view, so each format passes its wire key's bytes with
    // no `String` round-trip.
    // Parameter: key: ByteSlice (ArraySlice<u8>) at local 0.
    let mut locals = vec![param_local("__key", key_slice_type, false)];
    let mut next_local: u32 = 1;

    let mut stmts = Vec::new();

    // let __len = key.byte_len()
    let len_local = alloc_local(&mut next_local, &mut locals, TypeTable::I32);
    let len_expr = byte_slice_len_expr(local_ref(0, "__key", key_slice_type), span, compiler_items);
    stmts.push(let_mut_stmt("__len", len_local, TypeTable::I32, len_expr));

    // For each field, generate:
    //   if __len == N && key.get_byte(0) as i32 == B0 && ... { return Some(i); }
    // Positional fields are ordinal: skip them so they are never matched by
    // name (their values are bound via `positional_at`).
    for (i, (_, wire_name, _, _)) in fields.iter().enumerate() {
        if positional_flags.get(i).copied().unwrap_or(false) {
            continue;
        }
        let name_bytes = wire_name.as_bytes();
        let name_len = name_bytes.len() as i32;

        // Start with: __len == name_len
        let mut condition = i32_eq(
            local_ref(len_local, "__len", TypeTable::I32),
            i32_const(name_len),
            span,
        );

        // Chain: && key.get_byte(j) as i32 == byte_j
        for (j, &byte_val) in name_bytes.iter().enumerate() {
            let byte_check = i32_eq(
                key_get_byte_as_i32_expr(
                    local_ref(0, "__key", key_slice_type),
                    i32_const(j as i32),
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

    field_schema_method_fn(
        type_name,
        field_schema_trait,
        "lookup",
        "__key",
        key_slice_type,
        option_i32,
        locals,
        next_local,
        stmts,
        span,
    )
}

/// `impl FieldSchema for <Type> { fn positional_at(rank: i32) -> Option<i32> }`
/// — the static, per-type ordinal-field matcher. Maps the `rank`-th
/// `#[wire(positional)]` field (in declaration order) to its field index;
/// returns `null` for an out-of-range rank, and for every rank when the type
/// has no positional fields. `positional_flags` is aligned with the deserialize
/// loop's field indices, so the returned index drives the same `field == i`
/// assignment as `lookup`.
fn generate_positional_at_function(
    type_name: &FqTypeName,
    field_schema_trait: &str,
    positional_flags: &[bool],
    option_i32: TypeId,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirFunction {
    let locals = vec![param_local("__rank", TypeTable::I32, false)];
    let next_local: u32 = 1;

    let mut stmts = Vec::new();

    // For each positional field (in declaration order), generate:
    //   if __rank == R { return Some(field_index); }
    let mut rank: i32 = 0;
    for (field_index, &is_positional) in positional_flags.iter().enumerate() {
        if !is_positional {
            continue;
        }
        let condition = i32_eq(
            local_ref(0, "__rank", TypeTable::I32),
            i32_const(rank),
            span,
        );
        stmts.push(if_stmt(
            condition,
            block(vec![return_stmt(Some(option_some(
                i32_const(field_index as i32),
                option_i32,
                compiler_items,
            )))]),
            None,
        ));
        rank += 1;
    }
    stmts.push(return_stmt(Some(option_none(option_i32, compiler_items))));

    field_schema_method_fn(
        type_name,
        field_schema_trait,
        "positional_at",
        "__rank",
        TypeTable::I32,
        option_i32,
        locals,
        next_local,
        stmts,
        span,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground-truth heck outputs for an edge corpus. `core:serde::apply_case`
    /// (the library port used by reflect-based derivations) must match these
    /// exact strings — the same vectors are asserted in `serde_test.wado`. If
    /// heck changes, this fails first and the Wado vectors are updated in step.
    #[test]
    fn rename_all_edge_corpus_locks_heck_output() {
        let snake = |s: &str| apply_name_policy(s, "snake_case");
        assert_eq!(snake("userID"), "user_id");
        assert_eq!(snake("HTTPStatus"), "http_status");
        assert_eq!(snake("parseHTTP"), "parse_http");
        assert_eq!(snake("IOError"), "io_error");
        assert_eq!(snake("iOS"), "i_os");
        assert_eq!(snake("field2Name"), "field2_name");
        assert_eq!(snake("Apple2Banana"), "apple2_banana");
        assert_eq!(apply_name_policy("HTTPStatus", "camelCase"), "httpStatus");
        assert_eq!(apply_name_policy("userID", "PascalCase"), "UserId");
    }

    #[test]
    fn rename_all_from_snake_field() {
        assert_eq!(apply_name_policy("user_name", "camelCase"), "userName");
        assert_eq!(apply_name_policy("user_name", "snake_case"), "user_name");
        assert_eq!(apply_name_policy("user_name", "PascalCase"), "UserName");
        assert_eq!(apply_name_policy("user_name", "kebab-case"), "user-name");
        assert_eq!(
            apply_name_policy("user_name", "SCREAMING_SNAKE_CASE"),
            "USER_NAME"
        );
    }

    #[test]
    fn rename_all_from_pascal_case() {
        // PascalCase source, not the snake_case of struct fields.
        assert_eq!(apply_name_policy("AddRemote", "kebab-case"), "add-remote");
        assert_eq!(apply_name_policy("AddRemote", "snake_case"), "add_remote");
        assert_eq!(apply_name_policy("AddRemote", "camelCase"), "addRemote");
        assert_eq!(apply_name_policy("List", "kebab-case"), "list");
    }
}
