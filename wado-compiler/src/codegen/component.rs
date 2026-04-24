//! Component Model generator — builds a Wasm Component from a core module.
//!
//! This module contains all Component Model wrapping logic, handling:
//! - WASI interface imports
//! - Memory module
//! - Bundled modules (FTS, libm)
//! - Canonical intrinsics
//! - WASI function lowering
//! - Core module instantiation
//! - Canonical lifting for world exports
//! - HTTP handler export

use super::component_context::ComponentModelContext;
use super::postprocess;
use crate::ast::Type;
use crate::bundled::wado_bundled_libm_wasm;
use crate::component_model::{CmInstanceTypeGen, CmVariantCase, WasiFunctionInfo};
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{CanonicalIntrinsic, CmFuturePayload, CmScalarType, CmStreamPayload, WirPackage};
use wasm_encoder::{
    Alias, CanonicalOption, ComponentBuilder, ComponentExportKind, ComponentOuterAliasKind,
    ComponentValType, ExportKind, InstanceType, ModuleArg, PrimitiveValType, TypeBounds,
};

/// Build a complete Wasm Component from a pre-built core module and project metadata.
pub fn build_component(
    project: &FlatPackage,
    core_module: &[u8],
    wir_package: &WirPackage,
) -> Vec<u8> {
    let wasm_modules = &wir_package.wasm_modules;
    let mut builder = ComponentBuilder::default();
    let mut ctx = ComponentModelContext::new();

    // Generate WASI imports dynamically from registry
    generate_cm_imports(&mut builder, &mut ctx, project);

    // Type: result unit for run function (needed for task.return)
    let result_unit_type = ctx.register_type("result-unit");
    {
        let (_, enc) = builder.ty(Some("result-unit"));
        enc.defined_type().result(None, None);
    }

    // Kiln world types: defined inline at the component level (not
    // inside an imported instance) because `core:kiln/types` enters the
    // world scope via `use types.{...}`, not as an interface import.
    // The generator's single export `generate(raw: raw-request) ->
    // result<response, error>` plus its task-return canon need these
    // types defined before `emit_canonical_intrinsics` and
    // `emit_world_exports` run.
    if project.is_kiln_generator_world() {
        emit_kiln_world_types(&mut builder, &mut ctx);
    }

    // Bundled modules (FTS and libm) — computed from post-DCE imports
    let component_plan = &project.component_plan;
    let bundled_functions: Vec<String> = project
        .imports
        .iter()
        .filter(|i| i.namespace == "bundled")
        .map(|i| i.canonical_name.clone())
        .collect();

    // Core memory module
    let mem_info = wasm_modules.get("mem");
    let mem_module = build_memory_module(project.strip_names, mem_info, &bundled_functions);
    ctx.register_core_module("mem-mod");
    builder.core_module_raw(Some("mem-mod"), &mem_module);

    ctx.register_core_instance("mem");
    builder.core_instantiate(
        Some("mem"),
        ctx.core_module_idx("mem-mod"),
        Vec::<(&str, ModuleArg)>::new(),
    );

    ctx.set_memory(0);
    builder.core_alias_export(
        Some("memory"),
        ctx.core_instance_idx("mem"),
        "memory",
        ExportKind::Memory,
    );
    ctx.register_core_func("realloc");
    builder.core_alias_export(
        Some("realloc"),
        ctx.core_instance_idx("mem"),
        "realloc",
        ExportKind::Func,
    );

    embed_bundled_modules(&mut builder, &mut ctx, &bundled_functions);

    // Canonical intrinsics are discovered lazily during WIR translation via ensure_canonical().
    // They are stored in wir_package.needed_canonicals — the single source of truth.
    let all_canonical_intrinsics: Vec<CanonicalIntrinsic> =
        wir_package.needed_canonicals.iter().cloned().collect();

    // Build stream types needed by canonical intrinsics.
    let stream_types: IndexMap<CmStreamPayload, u32> = {
        let mut payloads: Vec<CmStreamPayload> = Vec::new();
        for intrinsic in &all_canonical_intrinsics {
            if let Some(p) = intrinsic.stream_payload()
                && !payloads.contains(&p)
            {
                payloads.push(p);
            }
        }
        let mut map = IndexMap::default();
        for payload in payloads {
            let (type_key, val_type) = match &payload {
                CmStreamPayload::U8 => (
                    "stream-u8".to_string(),
                    ComponentValType::Primitive(PrimitiveValType::U8),
                ),
                CmStreamPayload::Record(name) => {
                    // Alias the record type from the WASI interface that defines it.
                    // WASI imports are already generated (generate_cm_imports runs first),
                    // so we can alias the exported type from the interface instance.
                    let val = if let Some(interface_name) = project
                        .wasi_registry
                        .find_interface_for_struct_cm_name(name)
                    {
                        let inst_idx = ctx.instance_idx(&interface_name);
                        builder.alias_export(inst_idx, name, ComponentExportKind::Type);
                        let aliased_idx = ctx.register_type(name);
                        ComponentValType::Type(aliased_idx)
                    } else {
                        ComponentValType::Primitive(PrimitiveValType::U8)
                    };
                    (format!("stream-{name}"), val)
                }
            };
            ctx.register_type(&type_key);
            let (_, enc) = builder.ty(Some(&type_key));
            enc.defined_type().stream(Some(val_type));
            map.insert(payload, ctx.type_idx(&type_key));
        }
        map
    };

    // HTTP response types for future<T> canonical intrinsics
    let needs_trailers_future = all_canonical_intrinsics
        .iter()
        .any(|i| matches!(i.future_payload(), Some(CmFuturePayload::Trailers)));
    // Collect unique transmission sources (e.g., "cli", "filesystem")
    let transmission_sources: IndexSet<String> = all_canonical_intrinsics
        .iter()
        .filter_map(|i| {
            if let Some(CmFuturePayload::Transmission(ref source)) = i.future_payload() {
                Some(source.clone())
            } else {
                None
            }
        })
        .collect();

    // stream<u8> type is also needed by HTTP future types
    let stream_u8_type = stream_types
        .get(&CmStreamPayload::U8)
        .copied()
        .unwrap_or_else(|| {
            // If no stream<u8> intrinsics are needed, define it anyway for HTTP
            ctx.register_type("stream-u8");
            let (_, enc) = builder.ty(Some("stream-u8"));
            enc.defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
            ctx.type_idx("stream-u8")
        });

    let (trailers_future_type, transmission_future_types) = if needs_trailers_future {
        let (t, http_ft) = build_future_intrinsic_types(&mut builder, &mut ctx, stream_u8_type);
        let mut map: IndexMap<String, u32> = IndexMap::default();
        // HTTP build creates http-error-code based transmission future
        map.insert("http".to_string(), http_ft);
        // Build additional transmission types for other error-code sources
        for source in &transmission_sources {
            if source != "http" && !map.contains_key(source.as_str()) {
                let ft = build_transmission_future_type_for(&mut builder, &mut ctx, source);
                map.insert(source.clone(), ft);
            }
        }
        (t, map)
    } else if !transmission_sources.is_empty() {
        let mut map: IndexMap<String, u32> = IndexMap::default();
        for source in &transmission_sources {
            let ft = build_transmission_future_type_for(&mut builder, &mut ctx, source);
            map.insert(source.clone(), ft);
        }
        (0, map)
    } else {
        (0, IndexMap::default())
    };

    // Build scalar future types (e.g., future<s32>) from structured metadata
    let scalar_future_types =
        build_scalar_future_types(&mut builder, &mut ctx, &all_canonical_intrinsics);

    // Canonical intrinsics
    emit_canonical_intrinsics(
        &mut builder,
        &mut ctx,
        &all_canonical_intrinsics,
        &stream_types,
        result_unit_type,
        trailers_future_type,
        &transmission_future_types,
        &scalar_future_types,
        project.has_http_handler_export,
        project.is_kiln_generator_world(),
    );

    // Lower WASI functions
    lower_wasi_functions(project, &mut builder, &mut ctx);

    // Lower HTTP types functions
    if ctx.has_comp_func("http-fields-constructor") {
        ctx.register_core_func("http-fields-constructor");
        builder.lower_func(
            Some("http-fields-constructor"),
            ctx.comp_func_idx("http-fields-constructor"),
            [],
        );

        ctx.register_core_func("http-response-new");
        builder.lower_func(
            Some("http-response-new"),
            ctx.comp_func_idx("http-response-new"),
            [
                CanonicalOption::Memory(ctx.memory_idx()),
                CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
            ],
        );
    }

    // Collect available WASI functions
    let mut available_wasi_funcs: IndexSet<String> = IndexSet::default();
    for interface in project.wasi_registry.interfaces() {
        for func in &interface.functions {
            let local_name = func.local_alias_name();
            if ctx.has_core_func(&local_name) {
                available_wasi_funcs.insert(local_name);
            }
        }
    }

    // Embed core module
    ctx.register_core_module("main-mod");
    builder.core_module_raw(Some("main-mod"), core_module);

    // Build wasi instance
    let mut wasi_exports: Vec<(String, ExportKind, u32)> = Vec::new();
    for intrinsic in &all_canonical_intrinsics {
        let name = intrinsic.import_name();
        wasi_exports.push((name.clone(), ExportKind::Func, ctx.core_func_idx(&name)));
    }
    for local_name in &available_wasi_funcs {
        wasi_exports.push((
            local_name.clone(),
            ExportKind::Func,
            ctx.core_func_idx(local_name),
        ));
    }
    if (project.has_http_handler_export || project.has_effect("Client"))
        && ctx.has_core_func("http-fields-constructor")
    {
        wasi_exports.push((
            "http-fields-constructor".to_string(),
            ExportKind::Func,
            ctx.core_func_idx("http-fields-constructor"),
        ));
        wasi_exports.push((
            "http-response-new".to_string(),
            ExportKind::Func,
            ctx.core_func_idx("http-response-new"),
        ));
        wasi_exports.push((
            "wasi:http/Response::new".to_string(),
            ExportKind::Func,
            ctx.core_func_idx("http-response-new"),
        ));
    }

    let wasi_exports_refs: Vec<_> = wasi_exports
        .iter()
        .map(|(name, kind, idx)| (name.as_str(), *kind, *idx))
        .collect();
    let wasi_instance = builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports_refs);
    ctx.register_core_instance("wasi");

    // Build mem instance
    let mem_exports: Vec<(&str, ExportKind, u32)> = vec![
        ("memory", ExportKind::Memory, ctx.memory_idx()),
        ("realloc", ExportKind::Func, ctx.core_func_idx("realloc")),
    ];
    let mem_instance = builder.core_instantiate_exports(Some("mem-instance"), mem_exports);
    ctx.register_core_instance("mem-inst");

    // Build bundled instance
    let bundled_exports: Vec<(String, ExportKind, u32)> = bundled_functions
        .iter()
        .map(|func_name| {
            (
                func_name.clone(),
                ExportKind::Func,
                ctx.core_func_idx(func_name),
            )
        })
        .collect();

    let bundled_instance = if bundled_exports.is_empty() {
        None
    } else {
        let bundled_exports_refs: Vec<_> = bundled_exports
            .iter()
            .map(|(name, kind, idx)| (name.as_str(), *kind, *idx))
            .collect();
        let instance =
            builder.core_instantiate_exports(Some("bundled-instance"), bundled_exports_refs);
        ctx.register_core_instance("bundled");
        Some(instance)
    };

    // Instantiate core module
    ctx.register_core_instance("main");
    let mut main_args: Vec<(&str, ModuleArg)> = vec![
        ("wasi", ModuleArg::Instance(wasi_instance)),
        ("mem", ModuleArg::Instance(mem_instance)),
    ];
    if let Some(bundled_inst) = bundled_instance {
        main_args.push(("bundled", ModuleArg::Instance(bundled_inst)));
    }
    builder.core_instantiate(Some("main"), ctx.core_module_idx("main-mod"), main_args);

    // World exports
    emit_world_exports(
        &mut builder,
        &mut ctx,
        component_plan,
        result_unit_type,
        project.is_kiln_generator_world(),
    );

    if !project.strip_names {
        builder.append_names();
    }

    let mut component_bytes = builder.finish();

    if project.has_http_handler_export {
        append_http_handler_export(&mut component_bytes, &ctx, project);
    }

    component_bytes
}

fn wado_type_to_cm_primitive(ty: &Type) -> ComponentValType {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i8" => ComponentValType::Primitive(PrimitiveValType::S8),
            "i16" => ComponentValType::Primitive(PrimitiveValType::S16),
            "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
            "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
            "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
            "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
            "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
            "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
            "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
            "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
            "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
            "char" => ComponentValType::Primitive(PrimitiveValType::Char),
            "String" => ComponentValType::Primitive(PrimitiveValType::String),
            _ => panic!("unsupported Wado primitive type for CM: {}", named.name),
        },
        _ => panic!("unsupported Wado type for CM primitive: {ty:?}"),
    }
}

/// Like `wado_type_to_cm_primitive` but also resolves resource types using the
/// `own_resource_indices` map (resource Wado name → `own<resource>` type index).
fn type_to_cm_primitive_with_resources(
    ty: &Type,
    own_resource_indices: &IndexMap<String, u32>,
) -> ComponentValType {
    if let Type::Named(named) = ty
        && let Some(&own_idx) = own_resource_indices.get(&named.name)
    {
        return ComponentValType::Type(own_idx);
    }
    wado_type_to_cm_primitive(ty)
}

/// Build CM `ComponentValType` entries for tuple elements, handling `Stream<T>` and
/// `Future<T>` elements by emitting the necessary local types into `instance_type`.
///
/// Returns the `ComponentValType` list ready to pass to `instance_type.defined_type().tuple(...)`.
/// Emit a CM type definition for a Wado type, returning the `ComponentValType`.
///
/// Recursively defines complex CM types (stream, future, result, list, option, tuple)
/// inline in `instance_type`. Primitives and own resources are returned directly
/// without creating new type definitions.
///
/// This is the unified entry point for converting any Wado return type into a CM type.
fn emit_cm_val_type(
    ty: &Type,
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    error_code_idx: Option<u32>,
    has_local_error_code: bool,
    enum_export_indices: &IndexMap<String, u32>,
    own_resource_type_indices: &IndexMap<String, u32>,
    mut shared_type_gen: Option<&mut CmInstanceTypeGen>,
    project: Option<&FlatPackage>,
    ctx: &mut ComponentModelContext,
) -> ComponentValType {
    match ty {
        Type::Generic(g) if g.name == "Stream" => {
            let element = g.args.first().map(|inner| {
                emit_cm_val_type(
                    inner,
                    instance_type,
                    local_type_idx,
                    error_code_idx,
                    has_local_error_code,
                    enum_export_indices,
                    own_resource_type_indices,
                    shared_type_gen.as_deref_mut(),
                    project,
                    ctx,
                )
            });
            instance_type.ty().defined_type().stream(element);
            let idx = *local_type_idx;
            *local_type_idx += 1;
            ComponentValType::Type(idx)
        }
        Type::Generic(g) if g.name == "Future" => {
            let payload = g.args.first().map(|inner| {
                emit_cm_val_type(
                    inner,
                    instance_type,
                    local_type_idx,
                    error_code_idx,
                    has_local_error_code,
                    enum_export_indices,
                    own_resource_type_indices,
                    shared_type_gen.as_deref_mut(),
                    project,
                    ctx,
                )
            });
            instance_type.ty().defined_type().future(payload);
            let idx = *local_type_idx;
            *local_type_idx += 1;
            ComponentValType::Type(idx)
        }
        Type::Generic(g) if g.name == "Result" => {
            let err_idx = resolve_error_code_idx(
                instance_type,
                local_type_idx,
                error_code_idx,
                has_local_error_code,
                enum_export_indices,
                ctx,
            );
            let ok_type = if g.args.is_empty() {
                None
            } else {
                let ok = &g.args[0];
                if let Type::Named(named) = ok
                    && named.name == "()"
                {
                    None
                } else if let Type::Named(named) = ok
                    && own_resource_type_indices.contains_key(&named.name)
                {
                    Some(ComponentValType::Type(
                        own_resource_type_indices[&named.name],
                    ))
                } else if let (Some(type_gen), Some(proj)) = (shared_type_gen, project) {
                    // Complex ok types (records, options, variants, etc.) use shared type gen
                    type_gen.set_next_idx(*local_type_idx);
                    let resource_exports: IndexMap<&str, u32> = own_resource_type_indices
                        .iter()
                        .map(|(k, &v)| (k.as_str(), v))
                        .collect();
                    let ok_val = type_gen.ast_type_to_cm(
                        ok,
                        instance_type,
                        proj.wasi_registry,
                        &resource_exports,
                    );
                    *local_type_idx = type_gen.next_idx();
                    Some(ok_val)
                } else {
                    Some(type_to_cm_primitive_with_resources(
                        ok,
                        own_resource_type_indices,
                    ))
                }
            };
            instance_type
                .ty()
                .defined_type()
                .result(ok_type, Some(ComponentValType::Type(err_idx)));
            let idx = *local_type_idx;
            *local_type_idx += 1;
            ComponentValType::Type(idx)
        }
        Type::Generic(g) if g.name == "Array" && !g.args.is_empty() => {
            let element_val_type = emit_cm_val_type(
                &g.args[0],
                instance_type,
                local_type_idx,
                error_code_idx,
                has_local_error_code,
                enum_export_indices,
                own_resource_type_indices,
                shared_type_gen,
                project,
                ctx,
            );
            instance_type.ty().defined_type().list(element_val_type);
            let idx = *local_type_idx;
            *local_type_idx += 1;
            ComponentValType::Type(idx)
        }
        Type::Generic(g) if g.name == "Option" && !g.args.is_empty() => {
            let element_val_type = emit_cm_val_type(
                &g.args[0],
                instance_type,
                local_type_idx,
                error_code_idx,
                has_local_error_code,
                enum_export_indices,
                own_resource_type_indices,
                shared_type_gen,
                project,
                ctx,
            );
            instance_type.ty().defined_type().option(element_val_type);
            let idx = *local_type_idx;
            *local_type_idx += 1;
            ComponentValType::Type(idx)
        }
        Type::Generic(g) if g.name == "Tuple" && !g.args.is_empty() => {
            let tuple_types = build_cm_tuple_types(
                &g.args,
                instance_type,
                local_type_idx,
                error_code_idx,
                has_local_error_code,
                enum_export_indices,
                own_resource_type_indices,
                shared_type_gen.as_deref_mut(),
                project,
                ctx,
            );
            instance_type.ty().defined_type().tuple(tuple_types);
            let idx = *local_type_idx;
            *local_type_idx += 1;
            ComponentValType::Type(idx)
        }
        Type::Tuple(elems) if !elems.is_empty() => {
            let tuple_types = build_cm_tuple_types(
                elems,
                instance_type,
                local_type_idx,
                error_code_idx,
                has_local_error_code,
                enum_export_indices,
                own_resource_type_indices,
                shared_type_gen.as_deref_mut(),
                project,
                ctx,
            );
            instance_type.ty().defined_type().tuple(tuple_types);
            let idx = *local_type_idx;
            *local_type_idx += 1;
            ComponentValType::Type(idx)
        }
        _ => {
            // Check enum/variant export indices first (e.g. DescriptorType)
            if let Type::Named(named) = ty
                && let Some(&idx) = enum_export_indices.get(&named.name)
            {
                return ComponentValType::Type(idx);
            }
            // Complex types (e.g. WASI records like Instant) use shared type gen
            if let (Some(type_gen), Some(proj)) = (shared_type_gen, project) {
                type_gen.set_next_idx(*local_type_idx);
                let resource_exports: IndexMap<&str, u32> = own_resource_type_indices
                    .iter()
                    .map(|(k, &v)| (k.as_str(), v))
                    .collect();
                let val = type_gen.ast_type_to_cm(
                    ty,
                    instance_type,
                    proj.wasi_registry,
                    &resource_exports,
                );
                *local_type_idx = type_gen.next_idx();
                return val;
            }
            type_to_cm_primitive_with_resources(ty, own_resource_type_indices)
        }
    }
}

/// Resolve or create the error-code type index within an instance type.
fn resolve_error_code_idx(
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    error_code_idx: Option<u32>,
    has_local_error_code: bool,
    enum_export_indices: &IndexMap<String, u32>,
    ctx: &mut ComponentModelContext,
) -> u32 {
    if let Some(idx) = error_code_idx {
        idx
    } else if has_local_error_code && enum_export_indices.contains_key("ErrorCode") {
        enum_export_indices["ErrorCode"]
    } else {
        let outer_ec = ctx.type_idx("error-code");
        instance_type.alias(Alias::Outer {
            kind: ComponentOuterAliasKind::Type,
            count: 1,
            index: outer_ec,
        });
        let idx = *local_type_idx;
        *local_type_idx += 1;
        idx
    }
}

fn build_cm_tuple_types(
    elems: &[Type],
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    error_code_idx: Option<u32>,
    has_local_error_code: bool,
    enum_export_indices: &IndexMap<String, u32>,
    own_resource_type_indices: &IndexMap<String, u32>,
    mut shared_type_gen: Option<&mut CmInstanceTypeGen>,
    project: Option<&FlatPackage>,
    ctx: &mut ComponentModelContext,
) -> Vec<ComponentValType> {
    elems
        .iter()
        .map(|t| {
            emit_cm_val_type(
                t,
                instance_type,
                local_type_idx,
                error_code_idx,
                has_local_error_code,
                enum_export_indices,
                own_resource_type_indices,
                shared_type_gen.as_deref_mut(),
                project,
                ctx,
            )
        })
        .collect()
}

/// Collect resource type names referenced anywhere in a type tree.
///
/// Used to build the `needed_resources` list for `generate_cm_imports`.
fn collect_resources_in_type(
    ty: &Type,
    wasi_registry: &crate::component_model::WasiRegistry,
    out: &mut Vec<String>,
) {
    match ty {
        Type::Named(named)
            if named.source_interface.as_deref().is_some_and(|s| {
                s.starts_with("wasi:")
                    && wasi_registry
                        .get_resource_cm_name_by_source(s, &named.name)
                        .is_some()
            }) =>
        {
            if !out.contains(&named.name) {
                out.push(named.name.clone());
            }
        }
        Type::Generic(g) => {
            for arg in &g.args {
                collect_resources_in_type(arg, wasi_registry, out);
            }
        }
        Type::Tuple(elems) => {
            for elem in elems {
                collect_resources_in_type(elem, wasi_registry, out);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            collect_resources_in_type(inner, wasi_registry, out);
        }
        _ => {}
    }
}

fn wado_type_to_cm_val_type(
    _project: &FlatPackage,
    ty: &Type,
    stream_type_idx: Option<u32>,
    _error_code_idx: Option<u32>,
    result_param_type_idx: Option<u32>,
    enum_type_indices: &IndexMap<String, u32>,
    flags_type_indices: &IndexMap<String, u32>,
    borrow_resource_type_indices: &IndexMap<String, u32>,
) -> ComponentValType {
    match ty {
        Type::Named(named) => {
            if let Some(&enum_idx) = enum_type_indices.get(&named.name) {
                return ComponentValType::Type(enum_idx);
            }
            if let Some(&flags_idx) = flags_type_indices.get(&named.name) {
                return ComponentValType::Type(flags_idx);
            }
            match named.name.as_str() {
                "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
                "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
                "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
                "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
                "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
                "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
                "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
                "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
                "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
                "char" => ComponentValType::Primitive(PrimitiveValType::Char),
                "String" => ComponentValType::Primitive(PrimitiveValType::String),
                _ => panic!("unsupported Wado param type for CM: {}", named.name),
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            // borrow<resource> - WASI resource methods take self as &Resource
            if let Type::Named(named) = inner.as_ref()
                && let Some(&borrow_idx) = borrow_resource_type_indices.get(&named.name)
            {
                return ComponentValType::Type(borrow_idx);
            }
            panic!("unsupported reference param type for CM: {ty:?}")
        }
        Type::Generic(generic) => match generic.name.as_str() {
            "Stream" => ComponentValType::Type(stream_type_idx.expect("stream type not defined")),
            "Result" => ComponentValType::Type(
                result_param_type_idx.expect("result param type not defined"),
            ),
            _ => panic!("unsupported generic param type for CM: {}", generic.name),
        },
        _ => panic!("unsupported Wado param type for CM: {ty:?}"),
    }
}

fn build_memory_module(
    strip_names: bool,
    wasm_mod: Option<&crate::wir::WasmModuleInfo>,
    bundled_functions: &[String],
) -> Vec<u8> {
    let wasm_mod = wasm_mod.expect("core:allocator with #![wasm_module(\"mem\")] is required");

    // Determine minimum pages: at least 1, but must satisfy bundled module requirements.
    // libm requires its data section to fit in memory at instantiation time.
    let mut min_pages: u32 = 1;
    if !bundled_functions.is_empty() {
        let libm_pages = postprocess::extract_memory_min_pages(wado_bundled_libm_wasm());
        min_pages = min_pages.max(u32::try_from(libm_pages).unwrap_or(u32::MAX));
    }

    let memory = crate::wir::WirMemory {
        min: min_pages,
        max: None,
    };
    let mut wir = wasm_mod.to_wir_package(strip_names, memory);
    crate::wir_optimize::dce_unreachable_functions(&mut wir);
    super::emit::emit_core_module(&wir, strip_names)
}

fn embed_bundled_modules(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    bundled_functions: &[String],
) {
    if bundled_functions.is_empty() {
        return;
    }

    let libm_module =
        postprocess::convert_memory_to_import(wado_bundled_libm_wasm(), "env", "memory")
            .expect("Failed to process wado-bundled-libm module");

    let keep_exports: IndexSet<_> = bundled_functions.iter().cloned().collect();
    let final_module = postprocess::eliminate_dead_code(&libm_module, &keep_exports);

    ctx.register_core_module("libm-mod");
    builder.core_module_raw(Some("libm-mod"), &final_module);

    ctx.register_core_instance("libm-env");
    let libm_env_exports = [("memory", ExportKind::Memory, ctx.memory_idx())];
    let libm_env_instance =
        builder.core_instantiate_exports(Some("libm-env-instance"), libm_env_exports);

    ctx.register_core_instance("libm");
    builder.core_instantiate(
        Some("libm"),
        ctx.core_module_idx("libm-mod"),
        [("env", ModuleArg::Instance(libm_env_instance))],
    );

    for func_name in bundled_functions {
        ctx.register_core_func(func_name);
        builder.core_alias_export(
            Some(func_name),
            ctx.core_instance_idx("libm"),
            func_name,
            ExportKind::Func,
        );
    }
}

/// Build `future<result<_, error-code>>` (transmission) type without HTTP-specific types.
///
/// Used when only `CmFuturePayload::Transmission` is needed (e.g., `write_via_stream`)
/// but no HTTP types are imported.
/// Build `future<result<_, error-code>>` type for a specific `ErrorCode` source.
///
/// Different WASI interfaces (cli, filesystem, http, sockets) have different
/// error-code types, so each needs its own transmission future type.
fn build_transmission_future_type_for(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    source: &str,
) -> u32 {
    let error_code_key = if source == "cli" {
        "error-code".to_string()
    } else {
        format!("{source}-error-code")
    };
    let error_code_idx = if ctx.has_type(&error_code_key) {
        ctx.type_idx(&error_code_key)
    } else {
        // Fallback to CLI error-code if package-specific one is not available
        ctx.type_idx("error-code")
    };

    let result_key = format!("{source}-transmission-result");
    ctx.register_type(&result_key);
    {
        let (_, enc) = builder.ty(Some(&result_key));
        enc.defined_type()
            .result(None, Some(ComponentValType::Type(error_code_idx)));
    }

    let future_key = format!("{source}-transmission-future");
    ctx.register_type(&future_key);
    {
        let result_idx = ctx.type_idx(&result_key);
        let (_, enc) = builder.ty(Some(&future_key));
        enc.defined_type()
            .future(Some(ComponentValType::Type(result_idx)));
    }

    ctx.type_idx(&future_key)
}

/// Emit the `core:kiln/types` record/variant surface via an imported
/// instance. Called once per `core:kiln/generator` component.
///
/// Structure:
/// - Build an `InstanceType` whose interior defines `input-file`,
///   `output-file`, `response`, `error`, `raw-request` as records/variants
///   and exports each one by its WIT name. The CM validator's
///   `all_valtypes_named_in_defined` check requires records/variants
///   referenced by a component-level export to have their original type
///   ids in `exported_types`; the instance-wrap satisfies that because
///   inserting exported types into the set happens as the instance's
///   exports are processed in declaration order.
/// - Import that instance at the component level as
///   `core:kiln/types@0.1.0`. No runtime code is required — wasmtime
///   satisfies the import trivially because the instance has no function
///   members.
/// - Alias each exported type from the instance into the component's
///   local type index space so `emit_world_exports` and the canon
///   `task-return` routing can reference them by `ctx.type_idx(...)`.
/// - Also register a component-local `kiln-handler-result` =
///   `result<response, error>` (anonymous `result` doesn't need naming).
fn emit_kiln_world_types(builder: &mut ComponentBuilder, ctx: &mut ComponentModelContext) {
    let string_vt = ComponentValType::Primitive(PrimitiveValType::String);
    let bool_vt = ComponentValType::Primitive(PrimitiveValType::Bool);

    // Build the instance type. Indices below are instance-local; each
    // `ty()` call advances the instance's type counter, and each
    // `export(..Eq(idx)..)` creates an alias at the next counter slot.
    let mut instance_type = InstanceType::new();
    let mut local_idx: u32 = 0;

    // input-file
    instance_type
        .ty()
        .defined_type()
        .record([("path", string_vt), ("content", string_vt)]);
    let input_file_local = local_idx;
    local_idx += 1;
    instance_type.export(
        "input-file",
        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(input_file_local)),
    );
    let input_file_export = local_idx;
    local_idx += 1;

    // output-file
    instance_type.ty().defined_type().record([
        ("path", string_vt),
        ("content", string_vt),
        ("is-entry", bool_vt),
    ]);
    let output_file_local = local_idx;
    local_idx += 1;
    instance_type.export(
        "output-file",
        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(output_file_local)),
    );
    let output_file_export = local_idx;
    local_idx += 1;

    // list<output-file> — anonymous list referencing the exported alias.
    instance_type
        .ty()
        .defined_type()
        .list(ComponentValType::Type(output_file_export));
    let list_output_local = local_idx;
    local_idx += 1;

    // response = record { files: list<output-file> }
    instance_type
        .ty()
        .defined_type()
        .record([("files", ComponentValType::Type(list_output_local))]);
    let response_local = local_idx;
    local_idx += 1;
    instance_type.export(
        "response",
        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(response_local)),
    );
    #[allow(unused_assignments)]
    {
        local_idx += 1;
    }

    // error variant
    instance_type.ty().defined_type().variant([
        ("invalid-schema", Some(string_vt)),
        ("unsupported", Some(string_vt)),
        ("other", Some(string_vt)),
    ]);
    let error_local = local_idx;
    local_idx += 1;
    instance_type.export(
        "error",
        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(error_local)),
    );
    #[allow(unused_assignments)]
    {
        local_idx += 1;
    }

    // list<input-file>
    instance_type
        .ty()
        .defined_type()
        .list(ComponentValType::Type(input_file_export));
    let list_input_local = local_idx;
    local_idx += 1;

    // raw-request = record { primary: input-file, inputs: list<input-file>, options: string }
    instance_type.ty().defined_type().record([
        ("primary", ComponentValType::Type(input_file_export)),
        ("inputs", ComponentValType::Type(list_input_local)),
        ("options", string_vt),
    ]);
    let raw_request_local = local_idx;
    // No further local_idx uses after this point inside the instance
    // type; the export alias below advances the encoder's internal
    // counter but we don't reference it by index from here.
    instance_type.export(
        "raw-request",
        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(raw_request_local)),
    );

    // Register the instance type at the component level.
    let instance_type_idx = ctx.register_type("kiln-types-instance-type");
    {
        let (_, enc) = builder.ty(Some("kiln-types-instance-type"));
        enc.instance(&instance_type);
    }

    // Import the instance as `core:kiln/types@0.1.0`.
    ctx.register_instance("kiln-types");
    builder.import(
        "core:kiln/types@0.1.0",
        wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
    );

    // Alias each exported type into the component's local type space.
    // `ctx.register_type(name)` bumps the component's type counter in
    // lockstep with the encoder's so downstream registrations stay in
    // sync.
    for (export_name, local_name) in [
        ("input-file", "kiln-input-file"),
        ("output-file", "kiln-output-file"),
        ("response", "kiln-response"),
        ("error", "kiln-error"),
        ("raw-request", "kiln-raw-request"),
    ] {
        builder.alias_export(
            ctx.instance_idx("kiln-types"),
            export_name,
            ComponentExportKind::Type,
        );
        ctx.register_type(local_name);
    }

    // `result<response, error>` — anonymous, component-local. Used by
    // the canon `task-return` and by `emit_world_exports`.
    ctx.register_type("kiln-handler-result");
    {
        let response_idx = ctx.type_idx("kiln-response");
        let error_idx = ctx.type_idx("kiln-error");
        let (_, enc) = builder.ty(Some("kiln-handler-result"));
        enc.defined_type().result(
            Some(ComponentValType::Type(response_idx)),
            Some(ComponentValType::Type(error_idx)),
        );
    }
}

fn build_future_intrinsic_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    stream_u8_type: u32,
) -> (u32, u32) {
    ctx.register_type("http-fields");
    {
        let fields_resource_idx = ctx.type_idx("http-fields-resource");
        let (_, enc) = builder.ty(Some("http-fields"));
        enc.defined_type().own(fields_resource_idx);
    }

    ctx.register_type("http-option-stream-u8");
    {
        let (_, enc) = builder.ty(Some("http-option-stream-u8"));
        enc.defined_type()
            .option(ComponentValType::Type(stream_u8_type));
    }

    ctx.register_type("http-option-fields");
    {
        let fields_idx = ctx.type_idx("http-fields");
        let (_, enc) = builder.ty(Some("http-option-fields"));
        enc.defined_type()
            .option(ComponentValType::Type(fields_idx));
    }

    ctx.register_type("http-trailers-result");
    {
        let option_fields_idx = ctx.type_idx("http-option-fields");
        let error_code_idx = ctx.type_idx("http-error-code");
        let (_, enc) = builder.ty(Some("http-trailers-result"));
        enc.defined_type().result(
            Some(ComponentValType::Type(option_fields_idx)),
            Some(ComponentValType::Type(error_code_idx)),
        );
    }

    let trailers_future_type = ctx.register_type("http-trailers-future");
    {
        let trailers_result_idx = ctx.type_idx("http-trailers-result");
        let (_, enc) = builder.ty(Some("http-trailers-future"));
        enc.defined_type()
            .future(Some(ComponentValType::Type(trailers_result_idx)));
    }

    ctx.register_type("http-transmission-result");
    {
        let error_code_idx = ctx.type_idx("http-error-code");
        let (_, enc) = builder.ty(Some("http-transmission-result"));
        enc.defined_type()
            .result(None, Some(ComponentValType::Type(error_code_idx)));
    }

    ctx.register_type("http-transmission-future");
    {
        let transmission_result_idx = ctx.type_idx("http-transmission-result");
        let (_, enc) = builder.ty(Some("http-transmission-future"));
        enc.defined_type()
            .future(Some(ComponentValType::Type(transmission_result_idx)));
    }

    let transmission_future_type = ctx.type_idx("http-transmission-future");
    (trailers_future_type, transmission_future_type)
}

/// Build component-level `future<T>` types for scalar CM types (e.g., `future<s32>`).
///
/// Collects unique scalar payload types from the canonical intrinsics and registers
/// the corresponding component types. Returns a map from `CmScalarType` to type index.
fn build_scalar_future_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    canonical_intrinsics: &[CanonicalIntrinsic],
) -> IndexSet<(CmScalarType, u32)> {
    let mut scalars: IndexSet<CmScalarType> = IndexSet::default();
    for intrinsic in canonical_intrinsics {
        if let Some(CmFuturePayload::Scalar(scalar)) = intrinsic.future_payload() {
            scalars.insert(scalar);
        }
    }

    let mut result = IndexSet::default();
    for scalar in &scalars {
        let prim = cm_scalar_to_primitive(*scalar);
        let type_name = format!("future-{scalar}");
        let future_type = ctx.register_type(&type_name);
        {
            let (_, enc) = builder.ty(Some(&type_name));
            enc.defined_type()
                .future(Some(ComponentValType::Primitive(prim)));
        }
        result.insert((*scalar, future_type));
    }

    result
}

/// Convert a `CmScalarType` to `wasm_encoder::PrimitiveValType`.
fn cm_scalar_to_primitive(scalar: CmScalarType) -> PrimitiveValType {
    match scalar {
        CmScalarType::S8 => PrimitiveValType::S8,
        CmScalarType::S16 => PrimitiveValType::S16,
        CmScalarType::S32 => PrimitiveValType::S32,
        CmScalarType::S64 => PrimitiveValType::S64,
        CmScalarType::U8 => PrimitiveValType::U8,
        CmScalarType::U16 => PrimitiveValType::U16,
        CmScalarType::U32 => PrimitiveValType::U32,
        CmScalarType::U64 => PrimitiveValType::U64,
        CmScalarType::F32 => PrimitiveValType::F32,
        CmScalarType::F64 => PrimitiveValType::F64,
        CmScalarType::Bool => PrimitiveValType::Bool,
        CmScalarType::Char => PrimitiveValType::Char,
    }
}

fn emit_canonical_intrinsics(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    canonical_intrinsics: &[CanonicalIntrinsic],
    stream_types: &IndexMap<CmStreamPayload, u32>,
    result_unit_type: u32,
    trailers_future_type: u32,
    transmission_future_types: &IndexMap<String, u32>,
    scalar_future_types: &IndexSet<(CmScalarType, u32)>,
    has_http_handler_export: bool,
    is_kiln_generator: bool,
) {
    for intrinsic in canonical_intrinsics {
        ctx.register_core_func(&intrinsic.import_name());

        match intrinsic {
            CanonicalIntrinsic::StreamNew(payload) => {
                let st = stream_types[payload];
                builder.stream_new(st);
            }
            CanonicalIntrinsic::StreamWrite(payload) => {
                let st = stream_types[payload];
                builder.stream_write(
                    st,
                    [
                        CanonicalOption::Memory(ctx.memory_idx()),
                        CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                    ],
                );
            }
            CanonicalIntrinsic::StreamRead(payload) => {
                let st = stream_types[payload];
                builder.stream_read(
                    st,
                    [
                        CanonicalOption::Memory(ctx.memory_idx()),
                        CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                    ],
                );
            }
            CanonicalIntrinsic::StreamDropWritable(payload) => {
                builder.stream_drop_writable(stream_types[payload]);
            }
            CanonicalIntrinsic::StreamDropReadable(payload) => {
                builder.stream_drop_readable(stream_types[payload]);
            }
            CanonicalIntrinsic::StreamCancelRead(payload) => {
                builder.stream_cancel_read(stream_types[payload], false);
            }
            CanonicalIntrinsic::StreamCancelWrite(payload) => {
                builder.stream_cancel_write(stream_types[payload], false);
            }
            CanonicalIntrinsic::FutureNew(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                );
                builder.future_new(ft);
            }
            CanonicalIntrinsic::FutureWrite(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                );
                builder.future_write(
                    ft,
                    [
                        CanonicalOption::Async,
                        CanonicalOption::Memory(ctx.memory_idx()),
                        CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                    ],
                );
            }
            CanonicalIntrinsic::FutureRead(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                );
                builder.future_read(
                    ft,
                    [
                        CanonicalOption::Async,
                        CanonicalOption::Memory(ctx.memory_idx()),
                        CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                    ],
                );
            }
            CanonicalIntrinsic::FutureCancelRead(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                );
                builder.future_cancel_read(ft, false);
            }
            CanonicalIntrinsic::FutureCancelWrite(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                );
                builder.future_cancel_write(ft, false);
            }
            CanonicalIntrinsic::FutureDropWritable(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                );
                builder.future_drop_writable(ft);
            }
            CanonicalIntrinsic::FutureDropReadable(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                );
                builder.future_drop_readable(ft);
            }
            CanonicalIntrinsic::TaskReturn => {
                // Select the task-return result shape based on the
                // active world: `result<response, error>` for the kiln
                // generator, `http-handler-result` for HTTP service,
                // bare `result<>` otherwise. The flat decomposition of
                // this type must match the core module's task-return
                // import signature (computed via
                // `compute_export_flat_return_types`) or component
                // validation fails at instantiation.
                let task_return_type = if is_kiln_generator && ctx.has_type("kiln-handler-result") {
                    ctx.type_idx("kiln-handler-result")
                } else if has_http_handler_export && ctx.has_type("http-handler-result") {
                    ctx.type_idx("http-handler-result")
                } else {
                    result_unit_type
                };
                // task.return lifts payloads from linear memory into
                // component values; it does not allocate, so `realloc`
                // must not appear in the option list (wasm-tools
                // rejects it).
                builder.task_return(
                    Some(ComponentValType::Type(task_return_type)),
                    [CanonicalOption::Memory(ctx.memory_idx())],
                );
            }
            CanonicalIntrinsic::WaitableSetNew => {
                builder.waitable_set_new();
            }
            CanonicalIntrinsic::WaitableJoin => {
                builder.waitable_join();
            }
            CanonicalIntrinsic::WaitableSetWait => {
                builder.waitable_set_wait(false, ctx.memory_idx());
            }
            CanonicalIntrinsic::WaitableSetPoll => {
                builder.waitable_set_poll(false, ctx.memory_idx());
            }
            CanonicalIntrinsic::WaitableSetDrop => {
                builder.waitable_set_drop();
            }
            CanonicalIntrinsic::SubtaskDrop => {
                builder.subtask_drop();
            }
            CanonicalIntrinsic::SubtaskCancel => {
                builder.subtask_cancel(false);
            }
            CanonicalIntrinsic::ErrorContextNew => {
                builder.error_context_new([
                    CanonicalOption::UTF8,
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ]);
            }
            CanonicalIntrinsic::ErrorContextDebugMessage => {
                builder.error_context_debug_message([
                    CanonicalOption::UTF8,
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ]);
            }
            CanonicalIntrinsic::ErrorContextDrop => {
                builder.error_context_drop();
            }
        }
    }
}

/// Resolve the component-level type index for a future canonical intrinsic.
fn resolve_future_type(
    payload: CmFuturePayload,
    trailers_future_type: u32,
    transmission_future_types: &IndexMap<String, u32>,
    scalar_future_types: &IndexSet<(CmScalarType, u32)>,
) -> u32 {
    match payload {
        CmFuturePayload::Trailers => trailers_future_type,
        CmFuturePayload::Transmission(ref source) => {
            *transmission_future_types.get(source).unwrap_or_else(|| {
                panic!("transmission future type not registered for source: {source}")
            })
        }
        CmFuturePayload::Scalar(scalar) => scalar_future_types
            .iter()
            .find(|(s, _)| *s == scalar)
            .map(|(_, idx)| *idx)
            .expect("scalar future type not registered"),
    }
}

fn emit_world_exports(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    component_plan: &crate::wir_build::component_plan::ComponentPlan,
    result_unit_type: u32,
    is_kiln_generator: bool,
) {
    for export in &component_plan.world_exports {
        let core_name = format!("{}-core", export.name);
        let func_type_name = format!("{}-func-type", export.name);

        ctx.register_core_func(&core_name);
        builder.core_alias_export(
            Some(&core_name),
            ctx.core_instance_idx("main"),
            &export.name,
            ExportKind::Func,
        );

        let func_type = ctx.register_type(&func_type_name);
        {
            let (_, enc) = builder.ty(Some(&func_type_name));

            if export.is_http_handler {
                let request_type_idx = ctx.type_idx("http-request");
                let handler_result_type_idx = ctx.type_idx("http-handler-result");
                enc.function()
                    .async_(export.is_async)
                    .params([("request", ComponentValType::Type(request_type_idx))])
                    .result(Some(ComponentValType::Type(handler_result_type_idx)));
            } else if is_kiln_generator
                && ctx.has_type("kiln-raw-request")
                && ctx.has_type("kiln-handler-result")
            {
                let raw_request_idx = ctx.type_idx("kiln-raw-request");
                let handler_result_idx = ctx.type_idx("kiln-handler-result");
                enc.function()
                    .async_(export.is_async)
                    .params([("req", ComponentValType::Type(raw_request_idx))])
                    .result(Some(ComponentValType::Type(handler_result_idx)));
            } else {
                enc.function()
                    .async_(export.is_async)
                    .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                    .result(Some(ComponentValType::Type(result_unit_type)));
            }
        }

        ctx.register_comp_func(&export.name);
        let mut lift_opts = vec![
            CanonicalOption::Async,
            CanonicalOption::Memory(ctx.memory_idx()),
        ];
        // HTTP and Kiln exports pass CM-level records / lists / strings
        // through their params, so the canon must materialize them into
        // linear memory for the core function. `realloc` satisfies the
        // validator; `result_unit_type` exports take no params so stay
        // realloc-free.
        if export.is_http_handler || (is_kiln_generator && ctx.has_type("kiln-raw-request")) {
            lift_opts.push(CanonicalOption::Realloc(ctx.core_func_idx("realloc")));
        }
        builder.lift_func(
            Some(&export.name),
            ctx.core_func_idx(&core_name),
            func_type,
            lift_opts,
        );

        builder.export(
            &export.name,
            ComponentExportKind::Func,
            ctx.comp_func_idx(&export.name),
            None,
        );
        ctx.skip_comp_func_idx();
    }

    // Test exports
    for test in &component_plan.test_exports {
        let export_name = &test.export_name;
        let core_name = format!("{export_name}-core");
        let test_func_type_name = format!("{export_name}-func-type");

        ctx.register_core_func(&core_name);
        builder.core_alias_export(
            Some(&core_name),
            ctx.core_instance_idx("main"),
            &test.function_name,
            ExportKind::Func,
        );

        let test_func_type = ctx.register_type(&test_func_type_name);
        {
            let (_, enc) = builder.ty(Some(&test_func_type_name));
            enc.function()
                .async_(true)
                .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                .result(Some(ComponentValType::Type(result_unit_type)));
        }

        ctx.register_comp_func(export_name);
        builder.lift_func(
            Some(export_name),
            ctx.core_func_idx(&core_name),
            test_func_type,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(ctx.memory_idx()),
            ],
        );

        builder.export(
            export_name,
            ComponentExportKind::Func,
            ctx.comp_func_idx(export_name),
            None,
        );
        ctx.skip_comp_func_idx();
    }
}

/// Generate WASI imports dynamically from the registry.
fn generate_cm_imports(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &FlatPackage,
) {
    let cli_version = project
        .wasi_registry
        .get_cli_version()
        .expect("WASI CLI version not found in registry - lib/wasi/*.wado not loaded?");

    // Import wasi:cli/types for shared types (error-code)
    let cli_types_interface = format!("wasi:cli/types@{cli_version}");
    let error_code_cm_name = project
        .wasi_registry
        .get_enum_cm_name_by_interface(&cli_types_interface, "ErrorCode")
        .expect("ErrorCode CM name not found in wasi:cli/types");
    let error_code_variants = project
        .wasi_registry
        .get_enum_variants_by_interface(&cli_types_interface, "ErrorCode")
        .expect("ErrorCode enum not found in wasi:cli/types");
    let types_instance_type = ctx.register_type("types-instance-type");
    {
        let (_, enc) = builder.ty(Some("types-instance-type"));
        let mut instance_type = InstanceType::new();
        instance_type
            .ty()
            .defined_type()
            .enum_type(error_code_variants.iter().map(String::as_str));
        instance_type.export(
            error_code_cm_name,
            wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(0)),
        );
        enc.instance(&instance_type);
    }

    ctx.register_instance("types");
    let types_import_path = format!("wasi:cli/types@{cli_version}");
    builder.import(
        &types_import_path,
        wasm_encoder::ComponentTypeRef::Instance(types_instance_type),
    );

    ctx.register_type("error-code");
    builder.alias_export(
        ctx.instance_idx("types"),
        error_code_cm_name,
        ComponentExportKind::Type,
    );

    // Generate imports for each interface in the registry
    for interface_info in project.wasi_registry.interfaces() {
        if interface_info.interface == "run" {
            continue;
        }
        if interface_info.resource_type.is_some() {
            continue;
        }
        if interface_info.package == "http" {
            continue;
        }

        let supported_functions: Vec<_> = interface_info
            .functions
            .iter()
            .filter(|func| {
                if !project.wasi_registry.is_function_supported(func) {
                    return false;
                }
                // Use per-function check (same as wir_build) to avoid including
                // unused functions that reference unsupported types (e.g. Stream<u8>
                // in tuples when read_via_stream is not called).
                let func_key = format!("{}::{}", func.effect_name, func.method_name);
                project.used_wasi_functions.contains(&func_key)
            })
            .collect();

        if supported_functions.is_empty() {
            continue;
        }

        // Collect resource types referenced in any function signature.
        let mut needed_resources: Vec<String> = Vec::new();
        for func in &supported_functions {
            if let Some(ret_ty) = &func.return_type {
                collect_resources_in_type(ret_ty, project.wasi_registry, &mut needed_resources);
            }
            for (_, _, ty) in &func.params {
                collect_resources_in_type(ty, project.wasi_registry, &mut needed_resources);
            }
        }

        // Interfaces that reference resources DEFINED BY OTHER interfaces must be deferred
        // until those resource-defining interfaces are imported (via import_resource_using_interfaces,
        // Phase 3). Interfaces that define their own resources (source path == self) are handled here.
        let uses_external_resources = needed_resources.iter().any(|resource_name| {
            project
                .wasi_registry
                .get_resource_source_interface(resource_name)
                .is_some_and(|src| src != interface_info.path.as_str())
        });
        if uses_external_resources {
            continue;
        }

        let instance_type_name = format!("{}-instance-type", interface_info.interface);
        let instance_type_idx = ctx.register_type(&instance_type_name);
        {
            let (_, enc) = builder.ty(Some(&instance_type_name));
            let mut instance_type = InstanceType::new();
            let mut local_type_idx = 0u32;

            let mut resource_type_indices: IndexMap<String, u32> = IndexMap::default();
            let mut own_resource_type_indices: IndexMap<String, u32> = IndexMap::default();
            let mut borrow_resource_type_indices: IndexMap<String, u32> = IndexMap::default();
            for resource_name in &needed_resources {
                if let Some(source) = project
                    .wasi_registry
                    .find_wasi_resource_source(resource_name)
                    && let Some(cm_name) = project
                        .wasi_registry
                        .get_resource_cm_name_by_source(source, resource_name)
                {
                    instance_type.export(
                        cm_name,
                        wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
                    );
                    resource_type_indices.insert(resource_name.clone(), local_type_idx);
                    local_type_idx += 1;

                    let resource_idx = resource_type_indices[resource_name];
                    instance_type.ty().defined_type().own(resource_idx);
                    own_resource_type_indices.insert(resource_name.clone(), local_type_idx);
                    local_type_idx += 1;

                    instance_type.ty().defined_type().borrow(resource_idx);
                    borrow_resource_type_indices.insert(resource_name.clone(), local_type_idx);
                    local_type_idx += 1;
                }
            }

            // Determine if this interface defines its own ErrorCode (e.g. wasi:filesystem/types,
            // wasi:sockets/types) vs. using the shared wasi:cli/types error-code via outer alias.
            // ErrorCode may be an enum (cli/types) or a variant (filesystem/types, sockets/types).
            let has_local_error_code = project
                .wasi_registry
                .has_enum_in_interface(&interface_info.path, "ErrorCode")
                || project
                    .wasi_registry
                    .variants_for_interface(&interface_info.path)
                    .any(|(name, _, _)| name == "ErrorCode");

            /// Recursively collect Named types from a type tree.
            fn collect_named_types(ty: &Type, out: &mut Vec<String>) {
                match ty {
                    Type::Named(named) if named.name != "()" => {
                        if !out.contains(&named.name) {
                            out.push(named.name.clone());
                        }
                    }
                    Type::Generic(g) => {
                        for arg in &g.args {
                            collect_named_types(arg, out);
                        }
                    }
                    Type::Tuple(elems) => {
                        for elem in elems {
                            collect_named_types(elem, out);
                        }
                    }
                    _ => {}
                }
            }

            // Collect all named types referenced in function signatures
            let mut referenced_types: Vec<String> = Vec::new();
            for func in &supported_functions {
                for (_, _, ty) in &func.params {
                    collect_named_types(ty, &mut referenced_types);
                }
                if let Some(ret_ty) = &func.return_type {
                    collect_named_types(ret_ty, &mut referenced_types);
                }
            }

            // Partition into enums, variants, and flags by querying this
            // interface directly. Only types declared in
            // `interface_info.path` are emitted here; types that live in
            // other interfaces get their declarations from those interfaces'
            // own emit loop and are referenced via outer aliases.
            let iface = interface_info.path.as_str();
            let needed_variants: Vec<String> = referenced_types
                .iter()
                .filter(|name| {
                    project
                        .wasi_registry
                        .get_variant_cases_by_source(iface, name)
                        .is_some()
                        && (name.as_str() != "ErrorCode" || has_local_error_code)
                })
                .cloned()
                .collect();

            // Enums: exclude types that are also registered as variants (variant takes priority)
            let needed_enums: Vec<String> = referenced_types
                .iter()
                .filter(|name| {
                    project
                        .wasi_registry
                        .get_enum_variants_by_source(iface, name)
                        .is_some()
                        && !needed_variants.contains(name)
                        && (name.as_str() != "ErrorCode" || has_local_error_code)
                })
                .cloned()
                .collect();

            let needed_flags: Vec<String> = referenced_types
                .iter()
                .filter(|name| {
                    project
                        .wasi_registry
                        .get_flags_members_by_source(iface, name)
                        .is_some()
                })
                .cloned()
                .collect();

            let mut enum_type_indices: IndexMap<String, u32> = IndexMap::default();
            let mut enum_export_indices: IndexMap<String, u32> = IndexMap::default();
            let interface_path = &interface_info.path;

            for enum_name in &needed_enums {
                if let Some(variants) = project
                    .wasi_registry
                    .get_enum_variants_by_interface(interface_path, enum_name)
                {
                    instance_type
                        .ty()
                        .defined_type()
                        .enum_type(variants.iter().map(String::as_str));
                    let type_idx = local_type_idx;
                    local_type_idx += 1;
                    enum_type_indices.insert(enum_name.clone(), type_idx);

                    if let Some(cm_name) = project
                        .wasi_registry
                        .get_enum_cm_name_by_interface(interface_path, enum_name)
                    {
                        instance_type.export(
                            cm_name,
                            wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(type_idx)),
                        );
                        enum_export_indices.insert(enum_name.clone(), local_type_idx);
                        local_type_idx += 1;
                    }
                }
            }

            // Emit variant types in the instance type
            let mut variant_export_indices: IndexMap<String, u32> = IndexMap::default();
            // Build a map from wado_name → (cm_name, cases) for this interface's variants
            let interface_variants: IndexMap<String, (String, Vec<CmVariantCase>)> = project
                .wasi_registry
                .variants_for_interface(&interface_info.path)
                .map(|(wado_name, cm_name, cases)| {
                    (wado_name.to_string(), (cm_name.to_string(), cases.to_vec()))
                })
                .collect();
            // Shared CmInstanceTypeGen for complex types across variant payloads and functions.
            // Created early so variant payload types (e.g. Instant in NewTimestamp) are cached
            // and reused when the same types appear in function signatures. The
            // `interface_hint` lets `ast_type_to_cm` resolve ambiguous names
            // (e.g. `ErrorCode`, declared independently in multiple WASI
            // packages) against this emitter's owning interface.
            let mut shared_type_gen =
                CmInstanceTypeGen::with_interface_hint(local_type_idx, &interface_info.path);
            for (name, &idx) in &enum_export_indices {
                shared_type_gen.register_existing(&format!("enum:{name}"), idx);
            }
            for variant_name in &needed_variants {
                if let Some((_, cases)) = interface_variants.get(variant_name) {
                    // Build CM variant cases: (kebab-name, optional payload type)
                    let cm_cases: Vec<(&str, Option<ComponentValType>)> = cases
                        .iter()
                        .map(|c| {
                            let payload = c.payload.as_ref().map(|ty| {
                                shared_type_gen.set_next_idx(local_type_idx);
                                let val = emit_cm_val_type(
                                    ty,
                                    &mut instance_type,
                                    &mut local_type_idx,
                                    None,
                                    has_local_error_code,
                                    &enum_export_indices,
                                    &own_resource_type_indices,
                                    Some(&mut shared_type_gen),
                                    Some(project),
                                    ctx,
                                );
                                local_type_idx = shared_type_gen.next_idx().max(local_type_idx);
                                val
                            });
                            (c.cm_name.as_str(), payload)
                        })
                        .collect();
                    instance_type.ty().defined_type().variant(cm_cases);
                    let type_idx = local_type_idx;
                    local_type_idx += 1;

                    if let Some((variant_cm_name, _)) = interface_variants.get(variant_name) {
                        let cm_name: &str = variant_cm_name.as_str();
                        instance_type.export(
                            cm_name,
                            wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(type_idx)),
                        );
                        variant_export_indices.insert(variant_name.clone(), local_type_idx);
                        local_type_idx += 1;
                    }
                }
            }

            // Merge variant export indices into enum_export_indices so that
            // resolve_error_code_idx can find ErrorCode regardless of enum/variant.
            for (name, idx) in &variant_export_indices {
                enum_export_indices.insert(name.clone(), *idx);
            }

            // Emit flags types in the instance type (scoped to wasi:).
            let mut flags_export_indices: IndexMap<String, u32> = IndexMap::default();
            for flags_name in &needed_flags {
                let Some(source) = project.wasi_registry.find_wasi_flags_source(flags_name) else {
                    continue;
                };
                let source = source.to_string();
                if let Some(members) = project
                    .wasi_registry
                    .get_flags_members_by_source(&source, flags_name)
                {
                    instance_type
                        .ty()
                        .defined_type()
                        .flags(members.iter().map(String::as_str));
                    let type_idx = local_type_idx;
                    local_type_idx += 1;

                    if let Some(cm_name) = project
                        .wasi_registry
                        .get_flags_cm_name_by_source(&source, flags_name)
                    {
                        instance_type.export(
                            cm_name,
                            wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(type_idx)),
                        );
                        flags_export_indices.insert(flags_name.clone(), local_type_idx);
                        local_type_idx += 1;
                    }
                }
            }

            let mut deferred_func_exports: Vec<(String, u32)> = Vec::new();

            // Register flags and variant export indices into shared_type_gen
            for (name, &idx) in &flags_export_indices {
                shared_type_gen.register_existing(&format!("flags:{name}"), idx);
            }
            for (name, &idx) in &variant_export_indices {
                if let Some((variant_cm_name, _)) = interface_variants.get(name) {
                    shared_type_gen.register_existing(&format!("variant:{variant_cm_name}"), idx);
                }
            }

            for func in &supported_functions {
                // Pre-define param-only types (stream for params, result for params)
                let needs_stream_u8 = func
                    .params
                    .iter()
                    .any(|(_, _, ty)| matches!(ty, Type::Generic(g) if g.name == "Stream"));
                let needs_result_param = func
                    .params
                    .iter()
                    .any(|(_, _, ty)| matches!(ty, Type::Generic(g) if g.name == "Result"));

                let stream_type_idx = if needs_stream_u8 {
                    instance_type
                        .ty()
                        .defined_type()
                        .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                    let idx = local_type_idx;
                    local_type_idx += 1;
                    Some(idx)
                } else {
                    None
                };

                // error_code_idx is lazily resolved by emit_cm_val_type via resolve_error_code_idx,
                // but we still need it for param types that reference Result<_, ErrorCode>.
                let error_code_idx: Option<u32> = None;

                let result_param_type_idx = if needs_result_param {
                    instance_type.ty().defined_type().result(None, None);
                    let idx = local_type_idx;
                    local_type_idx += 1;
                    Some(idx)
                } else {
                    None
                };

                let kebab_params: Vec<(String, ComponentValType)> = func
                    .params
                    .iter()
                    .map(|(_, cm_name, ty)| {
                        let resolved_ty = project.wasi_registry.resolve_type(ty);
                        let val_type = if let Type::Named(named) = &resolved_ty
                            && named.source_interface.as_deref().is_some_and(|s| {
                                s.starts_with("wasi:")
                                    && project
                                        .wasi_registry
                                        .get_struct_fields_by_source(s, &named.name)
                                        .is_some()
                            }) {
                            shared_type_gen.set_next_idx(local_type_idx);
                            let resource_exports: IndexMap<&str, u32> = own_resource_type_indices
                                .iter()
                                .map(|(k, &v)| (k.as_str(), v))
                                .collect();
                            let val = shared_type_gen.ast_type_to_cm(
                                &resolved_ty,
                                &mut instance_type,
                                project.wasi_registry,
                                &resource_exports,
                            );
                            local_type_idx = shared_type_gen.next_idx();
                            val
                        } else {
                            wado_type_to_cm_val_type(
                                project,
                                ty,
                                stream_type_idx,
                                error_code_idx,
                                result_param_type_idx,
                                &enum_export_indices,
                                &flags_export_indices,
                                &borrow_resource_type_indices,
                            )
                        };
                        (cm_name.clone(), val_type)
                    })
                    .collect();
                let params: Vec<(&str, ComponentValType)> = kebab_params
                    .iter()
                    .map(|(name, val_type)| (name.as_str(), *val_type))
                    .collect();

                // Return type: use emit_cm_val_type for unified recursive type definition,
                // with shared_type_gen for struct (record) return types.
                let result_type = func.return_type.as_ref().map(|ty| {
                    let resolved_ty = project.wasi_registry.resolve_type(ty);
                    if let Type::Named(named) = &resolved_ty
                        && named.source_interface.as_deref().is_some_and(|s| {
                            s.starts_with("wasi:")
                                && project
                                    .wasi_registry
                                    .get_struct_fields_by_source(s, &named.name)
                                    .is_some()
                        })
                    {
                        shared_type_gen.set_next_idx(local_type_idx);
                        let resource_exports: IndexMap<&str, u32> = own_resource_type_indices
                            .iter()
                            .map(|(k, &v)| (k.as_str(), v))
                            .collect();
                        let val = shared_type_gen.ast_type_to_cm(
                            &resolved_ty,
                            &mut instance_type,
                            project.wasi_registry,
                            &resource_exports,
                        );
                        local_type_idx = shared_type_gen.next_idx();
                        val
                    } else {
                        emit_cm_val_type(
                            &resolved_ty,
                            &mut instance_type,
                            &mut local_type_idx,
                            error_code_idx,
                            has_local_error_code,
                            &enum_export_indices,
                            &own_resource_type_indices,
                            Some(&mut shared_type_gen),
                            Some(project),
                            ctx,
                        )
                    }
                });

                let mut func_encoder = instance_type.ty().function();
                if func.is_async {
                    func_encoder.async_(true).params(params).result(result_type);
                } else {
                    func_encoder.params(params).result(result_type);
                }

                let func_type_idx = local_type_idx;
                local_type_idx += 1;

                deferred_func_exports.push((func.wasi_func_name.clone(), func_type_idx));
            }

            for (func_name, func_type_idx) in &deferred_func_exports {
                instance_type.export(
                    func_name,
                    wasm_encoder::ComponentTypeRef::Func(*func_type_idx),
                );
            }

            enc.instance(&instance_type);
        }

        ctx.register_instance(&interface_info.interface);
        builder.import(
            &interface_info.path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        // Expose any resources defined in this interface at the outer component scope.
        // This allows other interfaces (e.g., wasi:filesystem/preopens which uses
        // wasi:filesystem/types::descriptor) to alias them via `alias outer`.
        for resource_name in &needed_resources {
            if let Some(source) = project
                .wasi_registry
                .find_wasi_resource_source(resource_name)
                && let Some(cm_name) = project
                    .wasi_registry
                    .get_resource_cm_name_by_source(source, resource_name)
            {
                let resource_type_name = format!("resource:{cm_name}");
                if !ctx.has_type(&resource_type_name) {
                    ctx.register_type(&resource_type_name);
                    builder.alias_export(
                        ctx.instance_idx(&interface_info.interface),
                        cm_name,
                        ComponentExportKind::Type,
                    );
                }
            }
        }

        // Expose ErrorCode from this interface at outer component scope.
        // Different interfaces (cli, filesystem, sockets) define different error-code types.
        // We register them with source-qualified keys (e.g., "filesystem-error-code").
        let interface_has_error_code = project
            .wasi_registry
            .has_enum_in_interface(&interface_info.path, "ErrorCode")
            || project
                .wasi_registry
                .variants_for_interface(&interface_info.path)
                .any(|(name, _, _)| name == "ErrorCode");
        if interface_has_error_code {
            let error_code_key = format!("{}-error-code", interface_info.package);
            if !ctx.has_type(&error_code_key) {
                ctx.register_type(&error_code_key);
                builder.alias_export(
                    ctx.instance_idx(&interface_info.interface),
                    "error-code",
                    ComponentExportKind::Type,
                );
            }
        }

        for func in &supported_functions {
            let local_name = project
                .wasi_registry
                .get_local_name(&interface_info.path, &func.wasi_func_name)
                .cloned()
                .unwrap_or_else(|| format!("{}-{}", interface_info.interface, func.wasi_func_name));

            ctx.register_comp_func(&local_name);
            builder.alias_export(
                ctx.instance_idx(&interface_info.interface),
                &func.wasi_func_name,
                ComponentExportKind::Func,
            );
        }
    }

    // Import interfaces with resource types
    import_interfaces_with_resources(builder, ctx, project);

    // Import wasi:http/types when the world exports an HTTP handler
    // or when the code uses the HTTP Client effect (e.g., CLI programs
    // that make outgoing HTTP requests).
    if project.has_http_handler_export || project.has_effect("Client") {
        import_http_types_for_service(project, builder, ctx);
    }

    // Import wasi:http/client if Client::send is used
    if project.has_effect("Client") && ctx.has_type("http-handler-result") {
        import_http_client(builder, ctx, project);
    }
}

fn import_http_types_for_service(
    project: &FlatPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
) {
    // Collect HTTP resources from the registry
    let http_resources: Vec<(String, String)> = project
        .wasi_registry
        .resources_for_interface("wasi:http/types")
        .map(|(wado, cm)| (wado.to_string(), cm.to_string()))
        .collect();

    let http_types_instance_type = ctx.register_type("http-types-instance-type");
    {
        let (_, enc) = builder.ty(Some("http-types-instance-type"));
        let mut instance_type = InstanceType::new();

        for (_, cm_name) in &http_resources {
            instance_type.export(
                cm_name,
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );
        }

        // Type generation starts after the SubResource exports.
        // CmInstanceTypeGen emits error-code and its payload structs
        // on demand when the parameter/return types are processed.
        // Use interface hint to disambiguate types shared across packages
        // (e.g., ErrorCode exists in http, filesystem, sockets).
        let resource_count = http_resources.len() as u32;
        let http_version = project
            .wasi_registry
            .get_package_version("http")
            .expect("WASI HTTP version not found in registry");
        let http_types_interface = format!("wasi:http/types@{http_version}");
        let mut type_gen =
            CmInstanceTypeGen::with_interface_hint(resource_count, &http_types_interface);
        let resource_exports: IndexMap<&str, u32> = http_resources
            .iter()
            .enumerate()
            .map(|(i, (_, cm_name))| (cm_name.as_str(), i as u32))
            .collect();

        let http_resource_names: IndexSet<&str> = http_resources
            .iter()
            .map(|(wado, _)| wado.as_str())
            .collect();

        let all_funcs: Vec<WasiFunctionInfo> = project
            .wasi_registry
            .interfaces()
            .find(|i| i.package == "http" && i.interface == "types")
            .map(|i| i.functions)
            .unwrap_or_default();

        // Emit constructor/static functions from registry metadata.
        // Processing their parameter and return types triggers on-demand emission of
        // all dependent types (error-code variant and its payload record types).
        let is_constructor_or_static = |f: &WasiFunctionInfo| {
            http_resource_names.contains(f.effect_name.as_str())
                && (f.wasi_func_name.starts_with("[constructor]")
                    || f.wasi_func_name.starts_with("[static]"))
        };
        for func in all_funcs.iter().filter(|f| is_constructor_or_static(f)) {
            let resolved_return = func
                .return_type
                .as_ref()
                .map(|ty| project.wasi_registry.resolve_type(ty));

            let cm_params: Vec<(String, ComponentValType)> = func
                .params
                .iter()
                .map(|(_, cm_name, ty)| {
                    let cm_type = type_gen.ast_type_to_cm(
                        ty,
                        &mut instance_type,
                        project.wasi_registry,
                        &resource_exports,
                    );
                    (cm_name.clone(), cm_type)
                })
                .collect();

            let cm_result = resolved_return.as_ref().map(|ty| {
                type_gen.ast_type_to_cm(
                    ty,
                    &mut instance_type,
                    project.wasi_registry,
                    &resource_exports,
                )
            });

            let param_refs: Vec<(&str, ComponentValType)> =
                cm_params.iter().map(|(n, t)| (n.as_str(), *t)).collect();
            let mut func_encoder = instance_type.ty().function();
            if func.is_async {
                func_encoder
                    .async_(true)
                    .params(param_refs)
                    .result(cm_result);
            } else {
                func_encoder.params(param_refs).result(cm_result);
            }
            let func_type_idx = type_gen.alloc_idx();

            instance_type.export(
                &func.wasi_func_name,
                wasm_encoder::ComponentTypeRef::Func(func_type_idx),
            );
        }

        let resource_methods: Vec<WasiFunctionInfo> = all_funcs
            .iter()
            .filter(|f| {
                // Skip functions already emitted in the constructor/static block
                if is_constructor_or_static(f) {
                    return false;
                }
                // Only include methods for known HTTP resources
                if !http_resource_names.contains(f.effect_name.as_str()) {
                    return false;
                }
                // Only include method/static functions
                let is_method_or_static = f.wasi_func_name.starts_with("[method]")
                    || f.wasi_func_name.starts_with("[static]");
                if !is_method_or_static {
                    return false;
                }
                // Only include functions that are actually used to avoid
                // referencing unsupported resource types (e.g. RequestOptions).
                project
                    .used_wasi_functions
                    .contains(&format!("{}::{}", f.effect_name, f.method_name))
            })
            .cloned()
            .collect();

        for func in &resource_methods {
            let resolved_return = func
                .return_type
                .as_ref()
                .map(|ty| project.wasi_registry.resolve_type(ty));

            let cm_params: Vec<(String, ComponentValType)> = func
                .params
                .iter()
                .map(|(_, cm_name, ty)| {
                    let cm_type = type_gen.ast_type_to_cm(
                        ty,
                        &mut instance_type,
                        project.wasi_registry,
                        &resource_exports,
                    );
                    (cm_name.clone(), cm_type)
                })
                .collect();

            let cm_result = resolved_return.as_ref().map(|ty| {
                type_gen.ast_type_to_cm(
                    ty,
                    &mut instance_type,
                    project.wasi_registry,
                    &resource_exports,
                )
            });

            let param_refs: Vec<(&str, ComponentValType)> =
                cm_params.iter().map(|(n, t)| (n.as_str(), *t)).collect();
            let mut func_encoder = instance_type.ty().function();
            if func.is_async {
                func_encoder
                    .async_(true)
                    .params(param_refs)
                    .result(cm_result);
            } else {
                func_encoder.params(param_refs).result(cm_result);
            }
            let func_type_idx = type_gen.alloc_idx();

            instance_type.export(
                &func.wasi_func_name,
                wasm_encoder::ComponentTypeRef::Func(func_type_idx),
            );
        }

        enc.instance(&instance_type);
    }

    ctx.register_instance("http-types");
    let http_version = project
        .wasi_registry
        .get_package_version("http")
        .expect("WASI HTTP version not found in registry");
    let http_types_import_path = format!("wasi:http/types@{http_version}");
    builder.import(
        &http_types_import_path,
        wasm_encoder::ComponentTypeRef::Instance(http_types_instance_type),
    );

    for (_, cm_name) in &http_resources {
        let local_name = format!("http-{cm_name}-resource");
        ctx.register_type(&local_name);
        builder.alias_export(
            ctx.instance_idx("http-types"),
            cm_name,
            ComponentExportKind::Type,
        );
    }
    // `ErrorCode` is defined in multiple wasi interfaces (filesystem, http,
    // sockets, sockets/ip-name-lookup) — bare-name lookup would shadow.
    // The HTTP error type is unambiguous: pin it to wasi:http/types via the
    // source-disambiguated lookup.
    let http_error_code_cm = project
        .wasi_registry
        .get_variant_cm_name_by_interface(&http_types_import_path, "ErrorCode")
        .or_else(|| {
            project
                .wasi_registry
                .get_enum_cm_name_by_interface(&http_types_import_path, "ErrorCode")
        })
        .expect("ErrorCode CM name not found for HTTP");
    ctx.register_type("http-error-code");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        http_error_code_cm,
        ComponentExportKind::Type,
    );

    // Alias constructor/static functions needed for lowering
    let fields_cm = project
        .wasi_registry
        .get_resource_cm_name("Fields")
        .unwrap();
    let response_cm = project
        .wasi_registry
        .get_resource_cm_name("Response")
        .unwrap();
    let constructor_fields = format!("[constructor]{fields_cm}");
    let static_response_new = format!("[static]{response_cm}.new");

    ctx.register_comp_func("http-fields-constructor");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        &constructor_fields,
        ComponentExportKind::Func,
    );
    ctx.register_comp_func("http-response-new");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        &static_response_new,
        ComponentExportKind::Func,
    );
    ctx.alias_comp_func("http-fields-constructor", "wasi:http/Fields::new");

    // Alias resource constructor/method/static functions for HTTP resources
    {
        let http_resource_names: IndexSet<&str> = http_resources
            .iter()
            .map(|(wado, _)| wado.as_str())
            .collect();
        let resource_funcs: Vec<(String, String)> = project
            .wasi_registry
            .interfaces()
            .find(|i| i.package == "http" && i.interface == "types")
            .map(|i| {
                i.functions
                    .iter()
                    .filter(|f| {
                        // Only include functions for known HTTP resources
                        if !http_resource_names.contains(f.effect_name.as_str()) {
                            return false;
                        }
                        // Skip Fields constructor and Response::new (handled above)
                        if f.wasi_func_name == constructor_fields
                            || f.wasi_func_name == static_response_new
                        {
                            return false;
                        }
                        // Only include constructor/method/static functions that are actually used
                        let is_resource_func = f.wasi_func_name.starts_with("[constructor]")
                            || f.wasi_func_name.starts_with("[method]")
                            || f.wasi_func_name.starts_with("[static]");
                        is_resource_func
                            && project
                                .used_wasi_functions
                                .contains(&format!("{}::{}", f.effect_name, f.method_name))
                    })
                    .map(|f| (f.wasi_func_name.clone(), f.local_alias_name()))
                    .collect()
            })
            .unwrap_or_default();
        for (cm_name, local_name) in &resource_funcs {
            ctx.register_comp_func(local_name);
            builder.alias_export(
                ctx.instance_idx("http-types"),
                cm_name,
                ComponentExportKind::Func,
            );
        }
    }

    // Define own<resource> types for each HTTP resource
    for (_, cm_name) in &http_resources {
        let resource_local = format!("http-{cm_name}-resource");
        let own_local = format!("http-{cm_name}");
        let resource_idx = ctx.type_idx(&resource_local);
        ctx.register_type(&own_local);
        let (_, enc) = builder.ty(Some(&own_local));
        enc.defined_type().own(resource_idx);
    }

    // Define result<own<response>, error-code>
    let response_type_idx = ctx.type_idx("http-response");
    let error_code_type_idx = ctx.type_idx("http-error-code");
    ctx.register_type("http-handler-result");
    {
        let (_, enc) = builder.ty(Some("http-handler-result"));
        enc.defined_type().result(
            Some(ComponentValType::Type(response_type_idx)),
            Some(ComponentValType::Type(error_code_type_idx)),
        );
    }
}

fn import_http_client(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &FlatPackage,
) {
    // Build the instance type for wasi:http/client from registry metadata.
    // The client interface references types defined in wasi:http/types (request, handler-result),
    // which are aliased from the outer scope.
    let request_type_idx = ctx.type_idx("http-request");
    let handler_result_type_idx = ctx.type_idx("http-handler-result");

    let client_iface = project
        .wasi_registry
        .interfaces()
        .find(|i| i.package == "http" && i.interface == "client")
        .expect("wasi:http/client interface not found in registry");
    let client_funcs = client_iface.functions;

    let instance_type_idx = ctx.register_type("http-client-instance-type");
    {
        let (_, enc) = builder.ty(Some("http-client-instance-type"));
        let mut instance_type = InstanceType::new();

        // Alias outer types needed by client functions.
        // local index 0 = request (param type), 1 = handler-result (return type)
        instance_type.alias(Alias::Outer {
            kind: ComponentOuterAliasKind::Type,
            count: 1,
            index: request_type_idx,
        });
        instance_type.alias(Alias::Outer {
            kind: ComponentOuterAliasKind::Type,
            count: 1,
            index: handler_result_type_idx,
        });

        // Emit each function from registry metadata.
        // Client functions use the same param/result types as the HTTP handler
        // (own<request> → result<own<response>, error-code>).
        // local_type_idx starts at 2: 0=request alias, 1=result alias
        for (func_type_idx, func) in (2_u32..).zip(client_funcs.iter()) {
            let mut func_encoder = instance_type.ty().function();
            if func.is_async {
                func_encoder
                    .async_(true)
                    .params([("request", ComponentValType::Type(0))])
                    .result(Some(ComponentValType::Type(1)));
            } else {
                func_encoder
                    .params([("request", ComponentValType::Type(0))])
                    .result(Some(ComponentValType::Type(1)));
            }

            instance_type.export(
                &func.wasi_func_name,
                wasm_encoder::ComponentTypeRef::Func(func_type_idx),
            );
        }

        enc.instance(&instance_type);
    }

    ctx.register_instance("http-client");
    let http_version = project
        .wasi_registry
        .get_package_version("http")
        .expect("WASI HTTP version not found in registry");
    let client_import_path = format!("wasi:http/client@{http_version}");
    builder.import(
        &client_import_path,
        wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
    );

    // Alias each function from the instance
    for func in &client_funcs {
        let local_name = project
            .wasi_registry
            .get_local_name(&client_import_path, &func.wasi_func_name)
            .cloned()
            .unwrap_or_else(|| format!("wasi:http/Client::{}", func.method_name));
        ctx.register_comp_func(&local_name);
        builder.alias_export(
            ctx.instance_idx("http-client"),
            &func.wasi_func_name,
            ComponentExportKind::Func,
        );
    }
}

fn import_interface_with_resource(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    interface_info: &crate::component_model::WasiInterfaceInfo,
    project: &FlatPackage,
) {
    let Some((_resource_wado_name, resource_cm_name)) = &interface_info.resource_type else {
        return;
    };

    if interface_info.package == "http" {
        return;
    }

    let Some(func) = interface_info.functions.first() else {
        return;
    };

    let local_name = func.local_alias_name();

    if !project.has_effect(&func.effect_name) || ctx.has_comp_func(&local_name) {
        return;
    }

    let outer_resource_type_name = format!("resource:{resource_cm_name}");
    let has_outer_resource = ctx.has_type(&outer_resource_type_name);

    let instance_type_name = format!("{}-instance-type", interface_info.interface);
    let instance_type_idx = ctx.register_type(&instance_type_name);
    {
        let (_, enc) = builder.ty(Some(&instance_type_name));
        let mut instance_type = InstanceType::new();

        if has_outer_resource {
            let outer_type_idx = ctx.type_idx(&outer_resource_type_name);
            instance_type.alias(Alias::Outer {
                kind: ComponentOuterAliasKind::Type,
                count: 1,
                index: outer_type_idx,
            });
        } else {
            instance_type.export(
                resource_cm_name,
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );
        }

        instance_type.ty().defined_type().own(0);
        instance_type
            .ty()
            .defined_type()
            .option(ComponentValType::Type(1));

        instance_type
            .ty()
            .function()
            .params::<[(&str, ComponentValType); 0], _>([])
            .result(Some(ComponentValType::Type(2)));

        instance_type.export(
            &func.wasi_func_name,
            wasm_encoder::ComponentTypeRef::Func(3),
        );

        enc.instance(&instance_type);
    }

    ctx.register_instance(&interface_info.interface);
    builder.import(
        &interface_info.path,
        wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
    );

    // When this interface defines its own resource (not aliased from a source interface),
    // expose the resource at the outer component scope so that other interfaces (like
    // wasi:filesystem/preopens) can alias it using `alias outer`.
    if !has_outer_resource {
        let resource_type_name = format!("resource:{resource_cm_name}");
        ctx.register_type(&resource_type_name);
        builder.alias_export(
            ctx.instance_idx(&interface_info.interface),
            resource_cm_name,
            ComponentExportKind::Type,
        );
    }

    ctx.register_comp_func(&local_name);
    builder.alias_export(
        ctx.instance_idx(&interface_info.interface),
        &func.wasi_func_name,
        ComponentExportKind::Func,
    );
}

fn import_interfaces_with_resources(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &FlatPackage,
) {
    let interfaces_with_resources: Vec<_> = project
        .wasi_registry
        .interfaces()
        .filter(|info| info.resource_type.is_some() && info.package != "http")
        .collect();

    // Phase 1: Import resource-defining interfaces
    let mut imported_source_interfaces: IndexSet<String> = IndexSet::default();
    for interface_info in &interfaces_with_resources {
        let Some((resource_wado_name, _resource_cm_name)) = &interface_info.resource_type else {
            continue;
        };

        let Some(source_path) = project
            .wasi_registry
            .get_resource_source_interface(resource_wado_name)
        else {
            continue;
        };

        if source_path == interface_info.path {
            continue;
        }

        let is_needed = interface_info.functions.first().is_some_and(|f| {
            project.has_effect(&f.effect_name) && !ctx.has_comp_func(&f.local_alias_name())
        });
        if !is_needed {
            continue;
        }

        if imported_source_interfaces.contains(source_path) {
            continue;
        }
        imported_source_interfaces.insert(source_path.to_string());

        let Some(resource_cm_name) = project
            .wasi_registry
            .get_resource_cm_name(resource_wado_name)
        else {
            continue;
        };

        let Some(cm_import) = crate::ast::CmImport::parse(source_path) else {
            continue;
        };

        // Check if this source interface defines ErrorCode
        let source_has_error_code = project
            .wasi_registry
            .variants_for_interface(source_path)
            .any(|(name, _, _)| name == "ErrorCode")
            || project
                .wasi_registry
                .has_enum_in_interface(source_path, "ErrorCode");

        let instance_type_name = format!("{}-instance-type", cm_import.interface);
        let instance_type_idx = ctx.register_type(&instance_type_name);
        #[allow(unused_assignments)]
        let mut local_type_idx = 0u32;
        {
            let (_, enc) = builder.ty(Some(&instance_type_name));
            let mut instance_type = InstanceType::new();
            instance_type.export(
                resource_cm_name,
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );
            local_type_idx += 1; // resource export

            // Include error-code export so it can be aliased at outer scope
            // for Transmission future types.
            if source_has_error_code {
                let interface_variants: Vec<_> = project
                    .wasi_registry
                    .variants_for_interface(source_path)
                    .filter(|(name, _, _)| *name == "ErrorCode")
                    .collect();
                if let Some((_, cm_name, cases)) = interface_variants.first() {
                    // Build variant cases inline
                    let cm_cases: Vec<(&str, Option<ComponentValType>)> = cases
                        .iter()
                        .map(|c| {
                            let payload = c.payload.as_ref().map(|_ty| {
                                // For simplicity, all payloads are option<string>
                                instance_type
                                    .ty()
                                    .defined_type()
                                    .option(ComponentValType::Primitive(PrimitiveValType::String));
                                let option_idx = local_type_idx;
                                local_type_idx += 1;
                                ComponentValType::Type(option_idx)
                            });
                            (c.cm_name.as_str(), payload)
                        })
                        .collect();
                    instance_type.ty().defined_type().variant(cm_cases);
                    let variant_idx = local_type_idx;
                    instance_type.export(
                        cm_name,
                        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(variant_idx)),
                    );
                }
            }
            enc.instance(&instance_type);
        }

        ctx.register_instance(&cm_import.interface);
        builder.import(
            source_path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        let resource_type_name = format!("resource:{resource_cm_name}");
        ctx.register_type(&resource_type_name);
        builder.alias_export(
            ctx.instance_idx(&cm_import.interface),
            resource_cm_name,
            ComponentExportKind::Type,
        );

        // Alias error-code at outer scope
        if source_has_error_code {
            let package = &cm_import.package;
            let error_code_key = format!("{package}-error-code");
            if !ctx.has_type(&error_code_key) {
                ctx.register_type(&error_code_key);
                builder.alias_export(
                    ctx.instance_idx(&cm_import.interface),
                    "error-code",
                    ComponentExportKind::Type,
                );
            }
        }
    }

    // Phase 2: Import function interfaces that use those resources
    for interface_info in &interfaces_with_resources {
        import_interface_with_resource(builder, ctx, interface_info, project);
    }

    // Register per-package error-code aliases for resource-defining interfaces.
    // These are needed by Transmission future types (future<result<_, error-code>>).
    for interface_info in &interfaces_with_resources {
        let has_error_code = project
            .wasi_registry
            .has_enum_in_interface(&interface_info.path, "ErrorCode")
            || project
                .wasi_registry
                .variants_for_interface(&interface_info.path)
                .any(|(name, _, _)| name == "ErrorCode");
        if has_error_code {
            let error_code_key = format!("{}-error-code", interface_info.package);
            if !ctx.has_type(&error_code_key) {
                ctx.register_type(&error_code_key);
                builder.alias_export(
                    ctx.instance_idx(&interface_info.interface),
                    "error-code",
                    ComponentExportKind::Type,
                );
            }
        }
    }

    // Phase 3: Import interfaces that reference resources from other interfaces
    // (e.g. wasi:filesystem/preopens whose get-directories returns a list of descriptors
    // from wasi:filesystem/types). These must be imported AFTER Phase 1 so that the
    // resource outer-aliases are available in ctx.
    import_resource_using_interfaces(builder, ctx, project);
}

/// Import interfaces that reference resources from other interfaces but don't define resources
/// themselves (e.g., wasi:filesystem/preopens which uses `descriptor` from wasi:filesystem/types).
///
/// Must run after Phase 1 of `import_interfaces_with_resources` so that `resource:*` types
/// have been registered in `ctx`.
fn import_resource_using_interfaces(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &FlatPackage,
) {
    for interface_info in project.wasi_registry.interfaces() {
        if interface_info.interface == "run" {
            continue;
        }
        if interface_info.resource_type.is_some() {
            continue;
        }
        if interface_info.package == "http" {
            continue;
        }

        let supported_functions: Vec<_> = interface_info
            .functions
            .iter()
            .filter(|func| {
                if !project.wasi_registry.is_function_supported(func) {
                    return false;
                }
                let func_key = format!("{}::{}", func.effect_name, func.method_name);
                project.used_wasi_functions.contains(&func_key)
            })
            .collect();

        if supported_functions.is_empty() {
            continue;
        }

        // Collect resources used in function signatures
        let mut needed_resources: Vec<String> = Vec::new();
        for func in &supported_functions {
            if let Some(ret_ty) = &func.return_type {
                collect_resources_in_type(ret_ty, project.wasi_registry, &mut needed_resources);
            }
            for (_, _, ty) in &func.params {
                collect_resources_in_type(ty, project.wasi_registry, &mut needed_resources);
            }
        }

        // Only handle interfaces that reference resources from other interfaces.
        // Interfaces with no resources are already handled in generate_cm_imports.
        if needed_resources.is_empty() {
            continue;
        }

        // Skip if already imported (e.g., previously handled by generate_cm_imports on a prior build)
        let first_func_local_name = supported_functions
            .first()
            .map(|f| f.local_alias_name())
            .unwrap_or_default();
        if ctx.has_comp_func(&first_func_local_name) {
            continue;
        }

        // Ensure all needed resources are imported before building the instance type.
        // A resource may appear only in a return type (e.g., preopens::get-directories
        // returns a list of descriptors) without any of its own methods being called.
        // In that case Phase 1/2 would not have imported the resource-defining interface,
        // so we do it here at component scope before entering the instance-type builder.
        // We use package-qualified names (e.g., "filesystem-types") to avoid collisions
        // with other interfaces that share the same short interface name (e.g., "cli/types").
        for resource_name in &needed_resources {
            if let Some(source) = project
                .wasi_registry
                .find_wasi_resource_source(resource_name)
                && let Some(cm_name) = project
                    .wasi_registry
                    .get_resource_cm_name_by_source(source, resource_name)
            {
                let outer_resource_type_name = format!("resource:{cm_name}");
                if ctx.has_type(&outer_resource_type_name) {
                    continue; // already imported
                }
                let Some(source_path) = project
                    .wasi_registry
                    .get_resource_source_interface(resource_name)
                else {
                    continue;
                };
                let Some(cm_import) = crate::ast::CmImport::parse(source_path) else {
                    continue;
                };
                // Use package-qualified names to avoid collision with same-named interfaces
                // from different packages (e.g., wasi:cli/types vs wasi:filesystem/types).
                let src_instance_name = format!("{}-{}", cm_import.package, cm_import.interface);
                let src_instance_type_name = format!("{src_instance_name}-instance-type");
                if !ctx.has_type(&src_instance_type_name) {
                    // Import the resource-defining interface minimally (just the resource export)
                    let src_instance_type_idx = ctx.register_type(&src_instance_type_name);
                    {
                        let (_, enc) = builder.ty(Some(&src_instance_type_name));
                        let mut src_it = InstanceType::new();
                        src_it.export(
                            cm_name,
                            wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
                        );
                        enc.instance(&src_it);
                    }
                    ctx.register_instance(&src_instance_name);
                    builder.import(
                        source_path,
                        wasm_encoder::ComponentTypeRef::Instance(src_instance_type_idx),
                    );
                }
                // Alias the resource type into the outer component scope
                ctx.register_type(&outer_resource_type_name);
                builder.alias_export(
                    ctx.instance_idx(&src_instance_name),
                    cm_name,
                    ComponentExportKind::Type,
                );
            }
        }

        // Build a map: resource_wado_name -> local_type_idx_in_instance_type
        // First alias outer resources, then build own<>/borrow<> types.
        let instance_type_name = format!("{}-instance-type", interface_info.interface);
        let instance_type_idx = ctx.register_type(&instance_type_name);
        {
            let (_, enc) = builder.ty(Some(&instance_type_name));
            let mut instance_type = InstanceType::new();
            let mut local_type_idx = 0u32;

            // Maps: resource_name -> (alias_local_idx, own_local_idx, borrow_local_idx)
            let mut resource_alias_indices: IndexMap<String, u32> = IndexMap::default();
            let mut own_resource_type_indices: IndexMap<String, u32> = IndexMap::default();
            let mut borrow_resource_type_indices: IndexMap<String, u32> = IndexMap::default();

            for resource_name in &needed_resources {
                if let Some(source) = project
                    .wasi_registry
                    .find_wasi_resource_source(resource_name)
                    && let Some(cm_name) = project
                        .wasi_registry
                        .get_resource_cm_name_by_source(source, resource_name)
                {
                    let outer_resource_type_name = format!("resource:{cm_name}");
                    if ctx.has_type(&outer_resource_type_name) {
                        let outer_idx = ctx.type_idx(&outer_resource_type_name);
                        {
                            // Alias the resource from the outer component scope
                            instance_type.alias(Alias::Outer {
                                kind: ComponentOuterAliasKind::Type,
                                count: 1,
                                index: outer_idx,
                            });
                            resource_alias_indices.insert(resource_name.clone(), local_type_idx);
                            local_type_idx += 1;

                            let resource_local_idx = resource_alias_indices[resource_name];
                            instance_type.ty().defined_type().own(resource_local_idx);
                            own_resource_type_indices.insert(resource_name.clone(), local_type_idx);
                            local_type_idx += 1;

                            instance_type.ty().defined_type().borrow(resource_local_idx);
                            borrow_resource_type_indices
                                .insert(resource_name.clone(), local_type_idx);
                            local_type_idx += 1;
                        }
                    } else {
                        // Resource not yet imported — skip this interface
                    }
                }
            }

            // Build function types using the aliased resource indices
            let mut deferred_func_exports: Vec<(String, u32)> = Vec::new();

            for func in &supported_functions {
                let result_type = func.return_type.as_ref().map(|ty| {
                    let resolved_ty = project.wasi_registry.resolve_type(ty);
                    emit_cm_val_type(
                        &resolved_ty,
                        &mut instance_type,
                        &mut local_type_idx,
                        None,
                        false,
                        &IndexMap::default(),
                        &own_resource_type_indices,
                        None,
                        None,
                        ctx,
                    )
                });

                let mut func_encoder = instance_type.ty().function();
                if func.is_async {
                    func_encoder
                        .async_(true)
                        .params::<[(&str, ComponentValType); 0], _>([])
                        .result(result_type);
                } else {
                    func_encoder
                        .params::<[(&str, ComponentValType); 0], _>([])
                        .result(result_type);
                }
                let func_type_idx = local_type_idx;
                local_type_idx += 1;

                deferred_func_exports.push((func.wasi_func_name.clone(), func_type_idx));
            }

            for (func_name, func_type_idx) in &deferred_func_exports {
                instance_type.export(
                    func_name,
                    wasm_encoder::ComponentTypeRef::Func(*func_type_idx),
                );
            }

            enc.instance(&instance_type);
        }

        ctx.register_instance(&interface_info.interface);
        builder.import(
            &interface_info.path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        for func in &supported_functions {
            let local_name = project
                .wasi_registry
                .get_local_name(&interface_info.path, &func.wasi_func_name)
                .cloned()
                .unwrap_or_else(|| format!("{}-{}", interface_info.interface, func.wasi_func_name));

            ctx.register_comp_func(&local_name);
            builder.alias_export(
                ctx.instance_idx(&interface_info.interface),
                &func.wasi_func_name,
                ComponentExportKind::Func,
            );
        }
    }
}

fn lower_wasi_functions(
    project: &FlatPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
) {
    for interface_info in project.wasi_registry.interfaces() {
        for func in &interface_info.functions {
            let local_name = func.local_alias_name();

            if !ctx.has_comp_func(&local_name) {
                continue;
            }

            ctx.register_core_func(&local_name);

            let mut options: Vec<CanonicalOption> = Vec::new();

            // Wado uses stackful sync lower for non-async imports (stream/future params
            // are passed as handles). Truly async imports (e.g., Client::send) use
            // canon lower async so that the caller can manage the subtask explicitly.
            if func.is_async {
                options.push(CanonicalOption::Async);
            }

            let needs_memory = func.needs_memory_with_registry(project.wasi_registry);
            let needs_realloc = needs_memory;

            if needs_memory {
                options.push(CanonicalOption::Memory(ctx.memory_idx()));
            }
            if needs_realloc {
                options.push(CanonicalOption::Realloc(ctx.core_func_idx("realloc")));
            }

            builder.lower_func(Some(&local_name), ctx.comp_func_idx(&local_name), options);
        }
    }
}

fn append_http_handler_export(
    component_bytes: &mut Vec<u8>,
    ctx: &ComponentModelContext,
    project: &FlatPackage,
) {
    use wasm_encoder::{ComponentExportSection, ComponentInstanceSection, ComponentSection};

    let handle_func_idx = ctx.comp_func_idx("handle");

    let request_cm = project
        .wasi_registry
        .get_resource_cm_name("Request")
        .unwrap();
    let response_cm = project
        .wasi_registry
        .get_resource_cm_name("Response")
        .unwrap();
    // Pin `ErrorCode` to wasi:http/types — same disambiguation as the
    // import side a few hundred lines up.
    let http_version_for_error = project
        .wasi_registry
        .get_package_version("http")
        .expect("WASI HTTP version not found in registry");
    let http_types_iface = format!("wasi:http/types@{http_version_for_error}");
    let error_code_cm = project
        .wasi_registry
        .get_variant_cm_name_by_interface(&http_types_iface, "ErrorCode")
        .or_else(|| {
            project
                .wasi_registry
                .get_enum_cm_name_by_interface(&http_types_iface, "ErrorCode")
        })
        .unwrap();

    let request_type_idx = ctx.type_idx(&format!("http-{request_cm}-resource"));
    let response_type_idx = ctx.type_idx(&format!("http-{response_cm}-resource"));
    let error_code_type_idx = ctx.type_idx("http-error-code");

    let mut instances = ComponentInstanceSection::new();
    instances.export_items([
        (request_cm, ComponentExportKind::Type, request_type_idx),
        (response_cm, ComponentExportKind::Type, response_type_idx),
        (
            error_code_cm,
            ComponentExportKind::Type,
            error_code_type_idx,
        ),
        ("handle", ComponentExportKind::Func, handle_func_idx),
    ]);

    let instance_idx = ctx.instance_count();

    let mut exports = ComponentExportSection::new();
    let http_version = project
        .wasi_registry
        .get_package_version("http")
        .expect("WASI HTTP version not found in registry");
    let handler_path = format!("wasi:http/handler@{http_version}");
    exports.export(
        &handler_path,
        ComponentExportKind::Instance,
        instance_idx,
        None,
    );

    instances.append_to_component(component_bytes);
    exports.append_to_component(component_bytes);
}
