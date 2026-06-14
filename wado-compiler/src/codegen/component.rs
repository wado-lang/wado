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

use super::component_context::{CmTypeKey, ComponentModelContext};
use super::postprocess;
use crate::ast::Type;
use crate::component_model::{CmFunctionInfo, CmInstanceTypeGen, CmVariantCase};
use crate::hashmap::{IndexMap, IndexSet};
use crate::nir_package::NirPackage;
use crate::wir::{CanonicalIntrinsic, CmFuturePayload, CmScalarType, CmStreamPayload, WirPackage};
use wasm_encoder::{
    Alias, CanonicalOption, ComponentBuilder, ComponentExportKind, ComponentOuterAliasKind,
    ComponentValType, ExportKind, InstanceType, ModuleArg, PrimitiveValType, TypeBounds,
};

/// Build a complete Wasm Component from a pre-built core module and project metadata.
pub fn build_component(
    project: &NirPackage,
    core_module: &[u8],
    wir_package: &WirPackage,
) -> Vec<u8> {
    let wasm_modules = &wir_package.wasm_modules;
    let mut builder = ComponentBuilder::default();
    let mut ctx = ComponentModelContext::new();

    // Generate CM imports. Which interfaces are imported, and in what category,
    // is decided by the WIR-level plan (the single source of truth); codegen
    // reads it rather than re-deriving, and its job is to encode each.
    generate_cm_imports(&mut builder, &mut ctx, project, &wir_package.import_plan);

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
    // `emit_world_exports` run. Gate on the world's `import KilnHost`
    // declaration so any kiln-generator-shaped world picks this path up
    // — the previous form matched `target_world == "core:kiln/generator"`
    // by string and missed future generator worlds.
    if project.world_imports_interface("KilnHost") {
        emit_kiln_world_types(&mut builder, &mut ctx);
    }

    // Imported wasm modules (e.g. libm.wat) — bytes are loaded by the
    // module loader and stored on the project, keyed by canonical
    // namespace string ("wasm:<path>"). Group post-DCE imports by their
    // namespace so each wasm module is embedded exactly once with the
    // union of exports actually referenced.
    let component_plan = &project.component_plan;
    let mut imported_wasm_module_uses: IndexMap<String, IndexSet<String>> = IndexMap::default();
    for imp in &project.imports {
        if imp.namespace.starts_with("wasm:") {
            imported_wasm_module_uses
                .entry(imp.namespace.clone())
                .or_default()
                .insert(imp.canonical_name.clone());
        }
    }

    // Core memory module
    let mem_info = wasm_modules.get("mem");
    let mem_module = build_memory_module(
        project.strip_names,
        mem_info,
        &imported_wasm_module_uses,
        &project.wasm_assets,
    );
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

    embed_imported_wasm_modules(
        &mut builder,
        &mut ctx,
        &imported_wasm_module_uses,
        &project.wasm_assets,
    );

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
                        .cm_interface_registry
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
        // The trailers/transmission futures belong to the resource-defining
        // interface that declares them (the HTTP types interface); derive its
        // package from the plan rather than hardcoding it.
        let pkg = wir_package
            .import_plan
            .iter()
            .find(|e| e.kind == crate::wir::ImportKind::ResourceDefiningInterface)
            .map(|e| crate::world_registry::fq_name_package(&e.fq).to_string())
            .expect("trailers future needs a resource-defining interface in the plan");
        let (t, defining_ft) =
            build_future_intrinsic_types(&mut builder, &mut ctx, stream_u8_type, &pkg);
        let mut map: IndexMap<String, u32> = IndexMap::default();
        map.insert(pkg.clone(), defining_ft);
        // Build additional transmission types for other error-code sources
        for source in &transmission_sources {
            if *source != pkg && !map.contains_key(source.as_str()) {
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
        project,
        &mut builder,
        &mut ctx,
        &all_canonical_intrinsics,
        &stream_types,
        result_unit_type,
        trailers_future_type,
        &transmission_future_types,
        &scalar_future_types,
        component_plan,
    );

    // Lower WASI functions
    lower_wasi_functions(project, &mut builder, &mut ctx);

    // Collect available WASI functions
    let mut available_wasi_funcs: IndexSet<String> = IndexSet::default();
    for interface in project.cm_interface_registry.interfaces() {
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

    // Build per-namespace instances for each imported wasm module.
    // The main module's `(import "<namespace>" "<export>" ...)` directives
    // use the canonical namespace string (e.g. `wasm:core:libm.wat`), so
    // each instance is keyed by the same string. Each
    // `core_instantiate_exports` call bumps the builder's instance
    // counter, so we must call `ctx.register_core_instance` in lockstep.
    let mut wasm_namespace_instances: Vec<(String, u32)> = Vec::new();
    for (namespace, used_exports) in &imported_wasm_module_uses {
        let instance_label = format!(
            "wasm-args-{}-instance",
            sanitise_wasm_namespace_for_label(namespace)
        );
        let exports: Vec<(String, ExportKind, u32)> = used_exports
            .iter()
            .map(|func_name| {
                (
                    func_name.clone(),
                    ExportKind::Func,
                    ctx.core_func_idx(func_name),
                )
            })
            .collect();
        if exports.is_empty() {
            continue;
        }
        let exports_refs: Vec<_> = exports
            .iter()
            .map(|(name, kind, idx)| (name.as_str(), *kind, *idx))
            .collect();
        let instance = builder.core_instantiate_exports(Some(&instance_label), exports_refs);
        ctx.register_core_instance(&instance_label);
        wasm_namespace_instances.push((namespace.clone(), instance));
    }

    // Instantiate core module
    ctx.register_core_instance("main");
    let mut main_args: Vec<(&str, ModuleArg)> = vec![
        ("wasi", ModuleArg::Instance(wasi_instance)),
        ("mem", ModuleArg::Instance(mem_instance)),
    ];
    for (namespace, instance) in &wasm_namespace_instances {
        main_args.push((namespace.as_str(), ModuleArg::Instance(*instance)));
    }
    builder.core_instantiate(Some("main"), ctx.core_module_idx("main-mod"), main_args);

    // World exports
    emit_world_exports(&mut builder, &mut ctx, component_plan, result_unit_type);

    // Test-name custom section: map each test export to its original (lossless)
    // name so `wado test` can display what the user wrote rather than the
    // ASCII-folded kebab export name. Emitted unconditionally for the test
    // world (even when empty) so the runner can rely on it being present.
    if !component_plan.test_exports.is_empty() {
        let payload = crate::test_names::encode(
            component_plan
                .test_exports
                .iter()
                .map(|t| (t.export_name.as_str(), t.original_name.as_deref())),
        );
        builder.custom_section(&wasm_encoder::CustomSection {
            name: crate::test_names::SECTION_NAME.into(),
            data: payload.into(),
        });
    }

    if !project.strip_names {
        builder.append_names();
    }

    let mut component_bytes = builder.finish();

    // Emit the per-interface CM instance exports (`wasi:cli/run`,
    // `wasi:http/handler`) for the lifted funcs `emit_world_exports` created;
    // freestanding world functions stay bare. See the function doc.
    append_interface_instance_exports(&mut component_bytes, &ctx, component_plan);

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
    project: Option<&NirPackage>,
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
            // E parameter: if it names a WASI resource that the enclosing
            // interface already brought in (via own_resource_type_indices),
            // use that own<R> handle directly. Otherwise fall back to the
            // canonical wasi:cli/types#error-code (covers the common case
            // where wasi WIT writes `result<_, error-code>`).
            let err_idx = if g.args.len() >= 2
                && let Type::Named(named_err) = &g.args[1]
                && let Some(&idx) = own_resource_type_indices.get(&named_err.name)
            {
                idx
            } else {
                resolve_error_code_idx(
                    instance_type,
                    local_type_idx,
                    error_code_idx,
                    has_local_error_code,
                    enum_export_indices,
                    ctx,
                )
            };
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
                        proj.cm_interface_registry,
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
        Type::Generic(g) if g.name == "List" && !g.args.is_empty() => {
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
            // Bare resource return types (e.g. `fn new() -> Connector`).
            // The Result/Option/etc. branches above already short-circuit
            // own resources by Wado-name; this path catches the case where
            // the resource is returned directly without a wrapper.
            if let Type::Named(named) = ty
                && let Some(&idx) = own_resource_type_indices.get(&named.name)
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
                    proj.cm_interface_registry,
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
    project: Option<&NirPackage>,
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
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
    out: &mut Vec<String>,
) {
    match ty {
        Type::Named(named)
            if named.source_interface.as_deref().is_some_and(|s| {
                s.starts_with("wasi:")
                    && cm_interface_registry
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
                collect_resources_in_type(arg, cm_interface_registry, out);
            }
        }
        Type::Tuple(elems) => {
            for elem in elems {
                collect_resources_in_type(elem, cm_interface_registry, out);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            collect_resources_in_type(inner, cm_interface_registry, out);
        }
        _ => {}
    }
}

fn wado_type_to_cm_val_type(
    _project: &NirPackage,
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
    imported_wasm_uses: &IndexMap<String, IndexSet<String>>,
    wasm_assets: &IndexMap<String, crate::loader::WasmAsset>,
) -> Vec<u8> {
    let wasm_mod = wasm_mod.expect("core:allocator with #![wasm_module(\"mem\")] is required");

    // Determine minimum pages: at least 1, but must satisfy any imported
    // wasm module's data-section requirements (e.g. libm's 17 pages).
    let mut min_pages: u32 = 1;
    for namespace in imported_wasm_uses.keys() {
        let asset = wasm_assets.get(namespace).unwrap_or_else(|| {
            panic!(
                "wasm asset for namespace {namespace:?} is referenced via #[canonical(...)] but \
                 was not registered via `use ... from \"<path>\" with {{ type: \"wat\"|\"wasm\" }}`",
            )
        });
        let pages = postprocess::extract_memory_min_pages(&asset.bytes);
        min_pages = min_pages.max(u32::try_from(pages).unwrap_or(u32::MAX));
    }

    let memory = crate::wir::WirMemory {
        min: min_pages,
        max: None,
    };
    let mut wir = wasm_mod.to_wir_package(strip_names, memory);
    // Memory-module DCE: BFS from exports/elements to mark unreachable
    // defined functions, mirror those indices into `dead_type_indices`
    // (the mem module has a 1:1 function/type correspondence — each
    // function owns its own func type at the same array index — so any
    // dead function's type is also dead), then run the generic
    // compaction.
    crate::wir_optimize::mark_unreachable_defined_functions(&mut wir);
    for &i in &wir.dead_func_indices.clone() {
        wir.dead_type_indices.insert(i);
    }
    crate::wir_optimize::compact_dead_items(&mut wir);
    // The memory/allocator module is hand-built and contains no `array.copy`,
    // so codegen feature flags do not apply here.
    super::emit::emit_core_module(
        &wir,
        strip_names,
        crate::codegen_flags::CodegenFlags::default(),
    )
}

/// Sanitise a wasm namespace string (e.g. `"wasm:core:libm.wat"`) into a
/// component-builder-safe instance/module name. Component names allow
/// only `[A-Za-z0-9_-]`, so colons and slashes are replaced with `_`.
fn sanitise_wasm_namespace_for_label(namespace: &str) -> String {
    namespace
        .strip_prefix("wasm:")
        .unwrap_or(namespace)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Embed each imported wasm module referenced by post-DCE TIR imports
/// (`namespace` prefixed with `"wasm:"`).
///
/// For each module: rewrite its memory definition into an import of
/// `env.memory`, prune to the union of actually-used exports, register
/// it in the component as a core module + instance, and alias each used
/// export so canonical-import lowerings can target it.
fn embed_imported_wasm_modules(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    imported_wasm_uses: &IndexMap<String, IndexSet<String>>,
    wasm_assets: &IndexMap<String, crate::loader::WasmAsset>,
) {
    if imported_wasm_uses.is_empty() {
        return;
    }
    let mut seen_aliased_funcs: IndexSet<String> = IndexSet::default();
    for (namespace, used_exports) in imported_wasm_uses {
        let asset = wasm_assets.get(namespace).unwrap_or_else(|| {
            panic!(
                "wasm asset for namespace {namespace:?} is referenced via #[canonical(...)] but \
                 was not registered via `use ... from \"<path>\" with {{ type: \"wat\"|\"wasm\" }}`",
            )
        });
        let stem = sanitise_wasm_namespace_for_label(namespace);
        let module_label = format!("wasm-mod-{stem}");
        let env_instance_label = format!("wasm-env-{stem}-instance");
        let instance_label = format!("wasm-{stem}");

        let imported = postprocess::convert_memory_to_import(&asset.bytes, "env", "memory")
            .unwrap_or_else(|e| panic!("failed to process wasm asset {namespace:?}: {e}"));
        let kept = postprocess::eliminate_dead_code(&imported, used_exports);

        ctx.register_core_module(&module_label);
        builder.core_module_raw(Some(&module_label), &kept);

        // Order matters: builder.core_instantiate{,_exports}() each create a
        // new core instance and bump the builder's instance counter. We
        // call `ctx.register_core_instance` immediately after each so the
        // `ctx`-side counter stays in lockstep with the builder's.
        let env_exports = [("memory", ExportKind::Memory, ctx.memory_idx())];
        let env_instance = builder.core_instantiate_exports(Some(&env_instance_label), env_exports);
        ctx.register_core_instance(&env_instance_label);

        builder.core_instantiate(
            Some(&instance_label),
            ctx.core_module_idx(&module_label),
            [("env", ModuleArg::Instance(env_instance))],
        );
        ctx.register_core_instance(&instance_label);

        for func_name in used_exports {
            // Two distinct namespaces could in theory export the same name.
            // The component builder keys core funcs by name, so use a
            // namespace-qualified label internally and alias under the
            // export name as well so canonical lowerings can locate it.
            if !seen_aliased_funcs.insert(func_name.clone()) {
                continue;
            }
            ctx.register_core_func(func_name);
            builder.core_alias_export(
                Some(func_name),
                ctx.core_instance_idx(&instance_label),
                func_name,
                ExportKind::Func,
            );
        }
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

    let result = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Result {
            ok: None,
            err: Some(Box::new(CmTypeKey::Leaf(error_code_idx))),
        },
        Some(&format!("{source}-transmission-result")),
    );
    intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Future(Box::new(CmTypeKey::Leaf(result))),
        Some(&format!("{source}-transmission-future")),
    )
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
/// - Also intern the component-local `result<response, error>` handler type,
///   which the export lift and `task-return` routing resolve by structure.
fn emit_kiln_world_types(builder: &mut ComponentBuilder, ctx: &mut ComponentModelContext) {
    let string_vt = ComponentValType::Primitive(PrimitiveValType::String);
    let bool_vt = ComponentValType::Primitive(PrimitiveValType::Bool);

    // Build the instance type. Each `ty()` call advances the instance's
    // type counter by one, and each `export(..Eq(idx)..)` creates an
    // alias at the next slot. The local helpers track that counter
    // explicitly so we can name the indices we actually reference down
    // the function and drop the rest without a dead `let _ = ...`.
    fn alloc(next_idx: &mut u32) -> u32 {
        let idx = *next_idx;
        *next_idx += 1;
        idx
    }
    fn emit_record(
        it: &mut InstanceType,
        next_idx: &mut u32,
        fields: &[(&'static str, ComponentValType)],
    ) -> u32 {
        it.ty().defined_type().record(fields.iter().copied());
        alloc(next_idx)
    }
    fn emit_variant(
        it: &mut InstanceType,
        next_idx: &mut u32,
        cases: &[(&'static str, Option<ComponentValType>)],
    ) -> u32 {
        it.ty().defined_type().variant(cases.iter().copied());
        alloc(next_idx)
    }
    fn emit_list(it: &mut InstanceType, next_idx: &mut u32, elem: ComponentValType) -> u32 {
        it.ty().defined_type().list(elem);
        alloc(next_idx)
    }
    fn emit_export(it: &mut InstanceType, next_idx: &mut u32, name: &str, eq_idx: u32) -> u32 {
        it.export(
            name,
            wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(eq_idx)),
        );
        alloc(next_idx)
    }

    let mut instance_type = InstanceType::new();
    let mut next_idx: u32 = 0;

    // input-file
    let input_file_local = emit_record(
        &mut instance_type,
        &mut next_idx,
        &[("path", string_vt), ("content", string_vt)],
    );
    let input_file_export = emit_export(
        &mut instance_type,
        &mut next_idx,
        "input-file",
        input_file_local,
    );

    // output-file
    let output_file_local = emit_record(
        &mut instance_type,
        &mut next_idx,
        &[
            ("path", string_vt),
            ("content", string_vt),
            ("is-entry", bool_vt),
        ],
    );
    let output_file_export = emit_export(
        &mut instance_type,
        &mut next_idx,
        "output-file",
        output_file_local,
    );

    // list<output-file> + response record
    let list_output_local = emit_list(
        &mut instance_type,
        &mut next_idx,
        ComponentValType::Type(output_file_export),
    );
    let response_local = emit_record(
        &mut instance_type,
        &mut next_idx,
        &[("files", ComponentValType::Type(list_output_local))],
    );
    emit_export(
        &mut instance_type,
        &mut next_idx,
        "response",
        response_local,
    );

    // error variant
    let error_local = emit_variant(
        &mut instance_type,
        &mut next_idx,
        &[
            ("invalid-schema", Some(string_vt)),
            ("unsupported", Some(string_vt)),
            ("other", Some(string_vt)),
        ],
    );
    emit_export(&mut instance_type, &mut next_idx, "error", error_local);

    // list<input-file> + raw-request record
    let list_input_local = emit_list(
        &mut instance_type,
        &mut next_idx,
        ComponentValType::Type(input_file_export),
    );
    let raw_request_local = emit_record(
        &mut instance_type,
        &mut next_idx,
        &[
            ("primary", ComponentValType::Type(input_file_export)),
            ("inputs", ComponentValType::Type(list_input_local)),
            ("options", string_vt),
        ],
    );
    // The raw-request export advances the encoder counter once more,
    // but we never reference that slot again so we drop the result.
    instance_type.export(
        "raw-request",
        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(raw_request_local)),
    );
    let _ = next_idx;

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

    let response_idx = ctx.type_idx("kiln-response");
    let error_idx = ctx.type_idx("kiln-error");
    intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Result {
            ok: Some(Box::new(CmTypeKey::Leaf(response_idx))),
            err: Some(Box::new(CmTypeKey::Leaf(error_idx))),
        },
        None,
    );
}

/// Intern a defined CM type by its structural [`CmTypeKey`], emitting it once.
/// Repeated calls with an equal key return the cached index without re-emitting.
/// `debug_name`, when given, names the emitted type and registers the name so
/// existing `ctx.type_idx(name)` lookups still resolve.
fn intern_cm_type(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    key: &CmTypeKey,
    debug_name: Option<&str>,
) -> u32 {
    if let CmTypeKey::Leaf(idx) = key {
        return *idx;
    }
    if let Some(idx) = ctx.intern_lookup(key) {
        return idx;
    }
    let resolved = match key {
        CmTypeKey::Leaf(_) => unreachable!("Leaf handled above"),
        CmTypeKey::Own(inner) => ResolvedCmType::Own(intern_cm_type(builder, ctx, inner, None)),
        CmTypeKey::Option(inner) => {
            ResolvedCmType::Option(intern_cm_type(builder, ctx, inner, None))
        }
        CmTypeKey::Future(inner) => {
            ResolvedCmType::Future(intern_cm_type(builder, ctx, inner, None))
        }
        CmTypeKey::Result { ok, err } => {
            let ok = ok.as_ref().map(|k| intern_cm_type(builder, ctx, k, None));
            let err = err.as_ref().map(|k| intern_cm_type(builder, ctx, k, None));
            ResolvedCmType::Result { ok, err }
        }
    };

    let idx = match debug_name {
        Some(name) => ctx.register_type(name),
        None => ctx.register_anon_type(),
    };
    let (_, enc) = builder.ty(debug_name);
    match resolved {
        ResolvedCmType::Own(resource) => {
            enc.defined_type().own(resource);
        }
        ResolvedCmType::Option(inner) => {
            enc.defined_type().option(ComponentValType::Type(inner));
        }
        ResolvedCmType::Future(inner) => {
            enc.defined_type()
                .future(Some(ComponentValType::Type(inner)));
        }
        ResolvedCmType::Result { ok, err } => {
            enc.defined_type().result(
                ok.map(ComponentValType::Type),
                err.map(ComponentValType::Type),
            );
        }
    }
    ctx.intern_record(key.clone(), idx);
    idx
}

enum ResolvedCmType {
    Own(u32),
    Option(u32),
    Future(u32),
    Result { ok: Option<u32>, err: Option<u32> },
}

fn build_future_intrinsic_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    stream_u8_type: u32,
    pkg: &str,
) -> (u32, u32) {
    let fields_resource_idx = ctx.type_idx(&format!("{pkg}-fields-resource"));
    let error_code = CmTypeKey::Leaf(ctx.type_idx(&format!("{pkg}-error-code")));

    let fields = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Own(Box::new(CmTypeKey::Leaf(fields_resource_idx))),
        Some(&format!("{pkg}-fields")),
    );

    intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Option(Box::new(CmTypeKey::Leaf(stream_u8_type))),
        Some(&format!("{pkg}-option-stream-u8")),
    );

    let option_fields = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Option(Box::new(CmTypeKey::Leaf(fields))),
        Some(&format!("{pkg}-option-fields")),
    );

    let trailers_result = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Result {
            ok: Some(Box::new(CmTypeKey::Leaf(option_fields))),
            err: Some(Box::new(error_code.clone())),
        },
        Some(&format!("{pkg}-trailers-result")),
    );

    let trailers_future_type = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Future(Box::new(CmTypeKey::Leaf(trailers_result))),
        Some(&format!("{pkg}-trailers-future")),
    );

    let transmission_result = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Result {
            ok: None,
            err: Some(Box::new(error_code)),
        },
        Some(&format!("{pkg}-transmission-result")),
    );

    let transmission_future_type = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Future(Box::new(CmTypeKey::Leaf(transmission_result))),
        Some(&format!("{pkg}-transmission-future")),
    );

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
    project: &NirPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    canonical_intrinsics: &[CanonicalIntrinsic],
    stream_types: &IndexMap<CmStreamPayload, u32>,
    result_unit_type: u32,
    trailers_future_type: u32,
    transmission_future_types: &IndexMap<String, u32>,
    scalar_future_types: &IndexSet<(CmScalarType, u32)>,
    component_plan: &crate::wir_build::component_plan::ComponentPlan,
) {
    // The `task.return` canon needs the export's CM-resolved result type.
    // Worlds with one boundary export (HTTP service `handle`, kiln
    // `generate`, CLI `Command::run`) all share this single-task assumption;
    // the test world emits zero world exports so we fall back to `result<>`.
    let task_return_type = component_plan
        .world_exports
        .first()
        .map(|export| cm_export_type_to_idx(ctx, &export.cm_result, result_unit_type))
        .unwrap_or(result_unit_type);
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
                // The `task.return` result shape is the active world
                // export's CM-resolved return type — `result<response,
                // error>` for the kiln generator and HTTP service,
                // `result<>` for CLI and the test
                // world. Computed once at function entry from
                // `component_plan.world_exports[0].cm_result` so the
                // code path here is uniform across worlds. The flat
                // decomposition must match the core module's task-return
                // import signature (computed via
                // `compute_export_flat_return_types`) or component
                // validation fails at instantiation.
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
            CanonicalIntrinsic::ResourceDrop(cm_name) => {
                // A resource-defining interface aliases its resource type as
                // `{pkg}-<cm>-resource`; other interfaces use `resource:<cm>`.
                let defining_key = project
                    .cm_interface_registry
                    .resource_source_by_cm_name(cm_name)
                    .map(|src| {
                        format!(
                            "{}-{cm_name}-resource",
                            crate::world_registry::fq_name_package(src)
                        )
                    })
                    .filter(|key| ctx.has_type(key));
                let type_idx = match defining_key {
                    Some(key) => ctx.type_idx(&key),
                    None => ctx.type_idx(&format!("resource:{cm_name}")),
                };
                builder.resource_drop(type_idx);
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

/// Resolve a plan-level [`CmExportType`] to a component type index registered
/// in `ctx`.
///
/// `Named` follows the `{pkg}-{cm_name}` convention that `import_http_types_for_service`
/// and `emit_kiln_world_types` register for resource own-handles and named
/// variants. `HandlerResult` resolves through the structural type interner from
/// its `ok`/`err` arms, so it shares the type those helpers interned without a
/// per-world name.
fn cm_export_type_to_idx(
    ctx: &ComponentModelContext,
    ty: &crate::wir_build::component_plan::CmExportType,
    result_unit_type: u32,
) -> u32 {
    use crate::wir_build::component_plan::CmExportType;
    match ty {
        CmExportType::Unit => result_unit_type,
        CmExportType::Named {
            interface_fq,
            cm_name,
            // A param/return references the value type (`{pkg}-{cm_name}`, i.e.
            // `own<resource>` for a resource), so the kind does not change the
            // lookup — unlike the re-export in `collect_type_items`.
            is_resource: _,
        } => {
            let pkg = crate::world_registry::fq_name_package(interface_fq);
            assert!(
                !pkg.is_empty(),
                "world export Named CM type interface `{interface_fq}` has no `scheme:pkg/...` shape",
            );
            let key = format!("{pkg}-{cm_name}");
            ctx.type_idx(&key)
        }
        CmExportType::HandlerResult { ok, err } => {
            let arm = |a: &CmExportType| match a {
                CmExportType::Unit => None,
                _ => Some(Box::new(CmTypeKey::Leaf(cm_export_type_to_idx(
                    ctx,
                    a,
                    result_unit_type,
                )))),
            };
            let key = CmTypeKey::Result {
                ok: arm(ok),
                err: arm(err),
            };
            ctx.intern_lookup(&key)
                .unwrap_or_else(|| panic!("handler-result type not interned: {key:?}"))
        }
    }
}

fn emit_world_exports(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    component_plan: &crate::wir_build::component_plan::ComponentPlan,
    result_unit_type: u32,
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

            // Resources and variants follow the `{pkg}-{cm-name}` naming
            // convention emitted by `import_wasi_http_types` /
            // `emit_kiln_world_types`; the synthesized `result<own<resp>, error>`
            // lives under `{world_pkg}-handler-result`. See [`CmExportType`].
            let param_idxs: Vec<(String, u32)> = export
                .cm_params
                .iter()
                .map(|(name, cm_ty)| {
                    (
                        name.clone(),
                        cm_export_type_to_idx(ctx, cm_ty, result_unit_type),
                    )
                })
                .collect();
            let param_refs: Vec<(&str, ComponentValType)> = param_idxs
                .iter()
                .map(|(n, idx)| (n.as_str(), ComponentValType::Type(*idx)))
                .collect();
            let result_idx = cm_export_type_to_idx(ctx, &export.cm_result, result_unit_type);
            enc.function()
                .async_(export.is_async)
                .params(param_refs)
                .result(Some(ComponentValType::Type(result_idx)));
        }

        ctx.register_comp_func(&export.name);
        // `realloc` is always supplied to the canon. The Wado runtime always
        // exports a `realloc` (the chosen allocator), and wasm-tools accepts
        // the option even on canons whose lift code never calls back into it
        // (verified by running the full e2e suite, including param-free CLI
        // `run` and `Result<(), ()>` exports).
        let lift_opts = vec![
            CanonicalOption::Async,
            CanonicalOption::Memory(ctx.memory_idx()),
            CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
        ];
        builder.lift_func(
            Some(&export.name),
            ctx.core_func_idx(&core_name),
            func_type,
            lift_opts,
        );

        // Interface exports go in an instance export (added by
        // `append_interface_instance_exports`), not bare; only freestanding world
        // functions (kiln `generate`) are exported bare here.
        if export.from_interface_fq.is_none() {
            builder.export(
                &export.name,
                ComponentExportKind::Func,
                ctx.comp_func_idx(&export.name),
                None,
            );
            ctx.skip_comp_func_idx();
        }
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
    project: &NirPackage,
    import_plan: &[crate::wir::ImportEntry],
) {
    use crate::wir::ImportKind;
    let has_kind = |kind: ImportKind| import_plan.iter().any(|e| e.kind == kind);
    let needs_cli_types = has_kind(ImportKind::SharedTypes);
    let cli_version = project
        .cm_interface_registry
        .get_cli_version()
        .expect("WASI CLI version not found in registry - lib/wasi/*.wado not loaded?");

    // Import wasi:cli/types for the shared `error-code` enum, when the plan says
    // it is needed (a used interface references it, or an async-export
    // transmission future resolves to the cli error-code). A pure-compute or
    // clock-only program drops this otherwise-dead import.
    if needs_cli_types {
        let cli_types_interface = format!("wasi:cli/types@{cli_version}");
        let error_code_cm_name = project
            .cm_interface_registry
            .get_enum_cm_name_by_interface(&cli_types_interface, "ErrorCode")
            .expect("ErrorCode CM name not found in wasi:cli/types");
        let error_code_variants = project
            .cm_interface_registry
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

        ctx.register_instance("cli-types");
        let types_import_path = format!("wasi:cli/types@{cli_version}");
        builder.import(
            &types_import_path,
            wasm_encoder::ComponentTypeRef::Instance(types_instance_type),
        );

        ctx.register_type("error-code");
        builder.alias_export(
            ctx.instance_idx("cli-types"),
            error_code_cm_name,
            ComponentExportKind::Type,
        );
    }

    // Generate imports for each interface in the registry. Membership comes
    // from the plan — codegen imports an interface iff the plan lists it (the
    // function-bearing, non-resource-using interfaces; resource-defining and
    // resource-using interfaces are handled by the later phases).
    for interface_info in project.cm_interface_registry.interfaces() {
        if interface_info.interface == "run" {
            continue;
        }
        if interface_info.resource_type.is_some() {
            continue;
        }
        // Membership: the plan lists this FQ as a function-bearing interface.
        // (The shared `wasi:cli/types` is `SharedTypes`, not `FunctionInterface`,
        // so it is correctly excluded from this loop and handled by Phase 0.)
        if !import_plan
            .iter()
            .any(|e| e.fq == interface_info.path && e.kind == ImportKind::FunctionInterface)
        {
            continue;
        }

        // Which functions of this interface to expose in its instance type.
        let supported_functions: Vec<_> = interface_info
            .functions
            .iter()
            .filter(|func| {
                if !project.cm_interface_registry.is_function_supported(func) {
                    return false;
                }
                let func_key = format!("{}::{}", func.interface_name, func.method_name);
                project.used_wasi_functions.contains(&func_key)
            })
            .collect();

        // Collect resource types referenced in any function signature. The plan
        // guarantees these resolve to resources this interface defines itself
        // (a signature touching an externally-defined resource would have been
        // categorized `ResourceUsingInterface` and handled by Phase 3 instead).
        let mut needed_resources: Vec<String> = Vec::new();
        for func in &supported_functions {
            if let Some(ret_ty) = &func.return_type {
                collect_resources_in_type(
                    ret_ty,
                    project.cm_interface_registry,
                    &mut needed_resources,
                );
            }
            for (_, _, ty) in &func.params {
                collect_resources_in_type(ty, project.cm_interface_registry, &mut needed_resources);
            }
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
                    .cm_interface_registry
                    .find_wasi_resource_source(resource_name)
                    && let Some(cm_name) = project
                        .cm_interface_registry
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
                .cm_interface_registry
                .has_enum_in_interface(&interface_info.path, "ErrorCode")
                || project
                    .cm_interface_registry
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
                        .cm_interface_registry
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
                        .cm_interface_registry
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
                        .cm_interface_registry
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
                    .cm_interface_registry
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
                        .cm_interface_registry
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
                .cm_interface_registry
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
                let Some(source) = project
                    .cm_interface_registry
                    .find_wasi_flags_source(flags_name)
                else {
                    continue;
                };
                let source = source.to_string();
                if let Some(members) = project
                    .cm_interface_registry
                    .get_flags_members_by_source(&source, flags_name)
                {
                    instance_type
                        .ty()
                        .defined_type()
                        .flags(members.iter().map(String::as_str));
                    let type_idx = local_type_idx;
                    local_type_idx += 1;

                    if let Some(cm_name) = project
                        .cm_interface_registry
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
                        let resolved_ty = project.cm_interface_registry.resolve_type(ty);
                        let val_type = if let Type::Named(named) = &resolved_ty
                            && named.source_interface.as_deref().is_some_and(|s| {
                                project
                                    .cm_interface_registry
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
                                project.cm_interface_registry,
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
                    let resolved_ty = project.cm_interface_registry.resolve_type(ty);
                    if let Type::Named(named) = &resolved_ty
                        && named.source_interface.as_deref().is_some_and(|s| {
                            project
                                .cm_interface_registry
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
                            project.cm_interface_registry,
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

        ctx.register_instance(&format!(
            "{}-{}",
            interface_info.package, interface_info.interface
        ));
        builder.import(
            &interface_info.path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        // Expose any resources defined in this interface at the outer component scope.
        // This allows other interfaces (e.g., wasi:filesystem/preopens which uses
        // wasi:filesystem/types::descriptor) to alias them via `alias outer`.
        for resource_name in &needed_resources {
            if let Some(source) = project
                .cm_interface_registry
                .find_wasi_resource_source(resource_name)
                && let Some(cm_name) = project
                    .cm_interface_registry
                    .get_resource_cm_name_by_source(source, resource_name)
            {
                let resource_type_name = format!("resource:{cm_name}");
                if !ctx.has_type(&resource_type_name) {
                    ctx.register_type(&resource_type_name);
                    builder.alias_export(
                        ctx.instance_idx(&format!(
                            "{}-{}",
                            interface_info.package, interface_info.interface
                        )),
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
            .cm_interface_registry
            .has_enum_in_interface(&interface_info.path, "ErrorCode")
            || project
                .cm_interface_registry
                .variants_for_interface(&interface_info.path)
                .any(|(name, _, _)| name == "ErrorCode");
        if interface_has_error_code {
            let error_code_key = format!("{}-error-code", interface_info.package);
            if !ctx.has_type(&error_code_key) {
                ctx.register_type(&error_code_key);
                builder.alias_export(
                    ctx.instance_idx(&format!(
                        "{}-{}",
                        interface_info.package, interface_info.interface
                    )),
                    "error-code",
                    ComponentExportKind::Type,
                );
            }
        }

        for func in &supported_functions {
            let local_name = project
                .cm_interface_registry
                .get_local_name(&interface_info.path, &func.wasi_func_name)
                .cloned()
                .unwrap_or_else(|| format!("{}-{}", interface_info.interface, func.wasi_func_name));

            ctx.register_comp_func(&local_name);
            builder.alias_export(
                ctx.instance_idx(&format!(
                    "{}-{}",
                    interface_info.package, interface_info.interface
                )),
                &func.wasi_func_name,
                ComponentExportKind::Func,
            );
        }
    }

    // Resource-defining interfaces (resources + their members + getter, e.g.
    // wasi:http/types) — emitted before the resource-using phase so their
    // resource and composite types are available to interfaces that consume them.
    for fq in import_plan
        .iter()
        .filter(|e| e.kind == ImportKind::ResourceDefiningInterface)
        .map(|e| e.fq.clone())
        .collect::<Vec<_>>()
    {
        import_resource_defining_interface(project, builder, ctx, &fq);
    }

    // Import interfaces with resource types (including resource-using interfaces
    // such as the HTTP client, which consume the resources/composites above).
    import_interfaces_with_resources(builder, ctx, project, import_plan);
}

fn handler_result_key(ctx: &ComponentModelContext, pkg: &str) -> CmTypeKey {
    CmTypeKey::Result {
        ok: Some(Box::new(CmTypeKey::Leaf(
            ctx.type_idx(&format!("{pkg}-response")),
        ))),
        err: Some(Box::new(CmTypeKey::Leaf(
            ctx.type_idx(&format!("{pkg}-error-code")),
        ))),
    }
}

fn import_resource_defining_interface(
    project: &NirPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    types_fq: &str,
) {
    let pkg = crate::world_registry::fq_name_package(types_fq).to_string();
    let types_prefix = types_fq.split('@').next().unwrap_or(types_fq).to_string();

    let http_resources: Vec<(String, String)> = project
        .cm_interface_registry
        .resources_for_interface(&types_prefix)
        .map(|(wado, cm)| (wado.to_string(), cm.to_string()))
        .collect();

    // Fetch the interface's functions once (`interfaces()` deep-clones them) and
    // reuse below. A miss means the plan/registry FQ disagree — fail loudly.
    let all_funcs: Vec<CmFunctionInfo> = project
        .cm_interface_registry
        .interfaces()
        .find(|i| i.path == types_fq)
        .unwrap_or_else(|| {
            panic!("CM types interface `{types_fq}` from the import plan not found in registry")
        })
        .functions;

    let instance_type_name = format!("{pkg}-types-instance-type");
    let http_types_instance_type = ctx.register_type(&instance_type_name);
    {
        let (_, enc) = builder.ty(Some(&instance_type_name));
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
        let mut type_gen = CmInstanceTypeGen::with_interface_hint(resource_count, types_fq);
        let resource_exports: IndexMap<&str, u32> = http_resources
            .iter()
            .enumerate()
            .map(|(i, (_, cm_name))| (cm_name.as_str(), i as u32))
            .collect();

        let http_resource_names: IndexSet<&str> = http_resources
            .iter()
            .map(|(wado, _)| wado.as_str())
            .collect();

        // Emit constructor/static functions from registry metadata.
        // Processing their parameter and return types triggers on-demand emission of
        // all dependent types (error-code variant and its payload record types).
        let is_constructor_or_static = |f: &CmFunctionInfo| {
            http_resource_names.contains(f.interface_name.as_str())
                && (f.wasi_func_name.starts_with("[constructor]")
                    || f.wasi_func_name.starts_with("[static]"))
        };
        for func in all_funcs.iter().filter(|f| is_constructor_or_static(f)) {
            let resolved_return = func
                .return_type
                .as_ref()
                .map(|ty| project.cm_interface_registry.resolve_type(ty));

            let cm_params: Vec<(String, ComponentValType)> = func
                .params
                .iter()
                .map(|(_, cm_name, ty)| {
                    let cm_type = type_gen.ast_type_to_cm(
                        ty,
                        &mut instance_type,
                        project.cm_interface_registry,
                        &resource_exports,
                    );
                    (cm_name.clone(), cm_type)
                })
                .collect();

            let cm_result = resolved_return.as_ref().map(|ty| {
                type_gen.ast_type_to_cm(
                    ty,
                    &mut instance_type,
                    project.cm_interface_registry,
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

        let resource_methods: Vec<CmFunctionInfo> = all_funcs
            .iter()
            .filter(|f| {
                // Skip functions already emitted in the constructor/static block
                if is_constructor_or_static(f) {
                    return false;
                }
                // Only include methods for known HTTP resources
                if !http_resource_names.contains(f.interface_name.as_str()) {
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
                    .contains(&format!("{}::{}", f.interface_name, f.method_name))
            })
            .cloned()
            .collect();

        for func in &resource_methods {
            let resolved_return = func
                .return_type
                .as_ref()
                .map(|ty| project.cm_interface_registry.resolve_type(ty));

            let cm_params: Vec<(String, ComponentValType)> = func
                .params
                .iter()
                .map(|(_, cm_name, ty)| {
                    let cm_type = type_gen.ast_type_to_cm(
                        ty,
                        &mut instance_type,
                        project.cm_interface_registry,
                        &resource_exports,
                    );
                    (cm_name.clone(), cm_type)
                })
                .collect();

            let cm_result = resolved_return.as_ref().map(|ty| {
                type_gen.ast_type_to_cm(
                    ty,
                    &mut instance_type,
                    project.cm_interface_registry,
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

    let types_instance = format!("{pkg}-types");
    ctx.register_instance(&types_instance);
    builder.import(
        types_fq,
        wasm_encoder::ComponentTypeRef::Instance(http_types_instance_type),
    );

    for (_, cm_name) in &http_resources {
        let local_name = format!("{pkg}-{cm_name}-resource");
        ctx.register_type(&local_name);
        builder.alias_export(
            ctx.instance_idx(&types_instance),
            cm_name,
            ComponentExportKind::Type,
        );
    }
    // `ErrorCode` is defined in multiple wasi interfaces (filesystem, http,
    // sockets, sockets/ip-name-lookup) — bare-name lookup would shadow.
    // Pin it to the imported types interface via the source-disambiguated lookup.
    let http_error_code_cm = project
        .cm_interface_registry
        .get_variant_cm_name_by_interface(types_fq, "ErrorCode")
        .or_else(|| {
            project
                .cm_interface_registry
                .get_enum_cm_name_by_interface(types_fq, "ErrorCode")
        })
        .expect("ErrorCode CM name not found for the types interface");
    ctx.register_type(&format!("{pkg}-error-code"));
    builder.alias_export(
        ctx.instance_idx(&types_instance),
        http_error_code_cm,
        ComponentExportKind::Type,
    );

    // Alias the used constructors/methods/statics of this interface's resources.
    // Each is registered under its local alias name and lowered generically by
    // `lower_wasi_functions` (which derives the canonical options from the
    // signature), so there is no per-constructor special case.
    {
        let http_resource_names: IndexSet<&str> = http_resources
            .iter()
            .map(|(wado, _)| wado.as_str())
            .collect();
        let resource_funcs: Vec<(String, String)> = all_funcs
            .iter()
            .filter(|f| {
                if !http_resource_names.contains(f.interface_name.as_str()) {
                    return false;
                }
                let is_resource_func = f.wasi_func_name.starts_with("[constructor]")
                    || f.wasi_func_name.starts_with("[method]")
                    || f.wasi_func_name.starts_with("[static]");
                is_resource_func
                    && project
                        .used_wasi_functions
                        .contains(&format!("{}::{}", f.interface_name, f.method_name))
            })
            .map(|f| (f.wasi_func_name.clone(), f.local_alias_name()))
            .collect();
        for (cm_name, local_name) in &resource_funcs {
            ctx.register_comp_func(local_name);
            builder.alias_export(
                ctx.instance_idx(&types_instance),
                cm_name,
                ComponentExportKind::Func,
            );
        }
    }

    for (_, cm_name) in &http_resources {
        let resource_idx = ctx.type_idx(&format!("{pkg}-{cm_name}-resource"));
        intern_cm_type(
            builder,
            ctx,
            &CmTypeKey::Own(Box::new(CmTypeKey::Leaf(resource_idx))),
            Some(&format!("{pkg}-{cm_name}")),
        );
    }

    // Intern the `result<own<response>, error-code>` handler composite when this
    // interface provides both arms (the handler/world-export shape). The export
    // lift and any client interface resolve it by structure.
    if ctx.has_type(&format!("{pkg}-response")) && ctx.has_type(&format!("{pkg}-error-code")) {
        let key = handler_result_key(ctx, &pkg);
        intern_cm_type(builder, ctx, &key, None);
    }
}

/// Resolve a function signature type to a component-level type index, reusing
/// the resource own-handles and composites a resource-defining interface already
/// emitted (`{pkg}-{cm}`, `{pkg}-error-code`, and the interned `result<...>`).
/// Used by composite resource-using interfaces (e.g. the HTTP client) whose
/// signatures reference another interface's resources and error composite.
fn component_type_idx_for_signature_type(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    ty: &Type,
) -> u32 {
    let registry = project.cm_interface_registry;
    match ty {
        Type::Generic(g) if g.name == "AsyncCall" && g.args.len() == 1 => {
            component_type_idx_for_signature_type(builder, ctx, project, &g.args[0])
        }
        Type::Generic(g) if g.name == "Result" && g.args.len() == 2 => {
            let ok = component_type_idx_for_signature_type(builder, ctx, project, &g.args[0]);
            let err = component_type_idx_for_signature_type(builder, ctx, project, &g.args[1]);
            intern_cm_type(
                builder,
                ctx,
                &CmTypeKey::Result {
                    ok: Some(Box::new(CmTypeKey::Leaf(ok))),
                    err: Some(Box::new(CmTypeKey::Leaf(err))),
                },
                None,
            )
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            component_type_idx_for_signature_type(builder, ctx, project, inner)
        }
        Type::Named(named) => {
            if let Some(source) = registry.get_resource_source_interface(&named.name)
                && let Some(cm) = registry.get_resource_cm_name_by_source(source, &named.name)
            {
                let pkg = crate::world_registry::fq_name_package(source);
                return ctx.type_idx(&format!("{pkg}-{cm}"));
            }
            let source = named.source_interface.as_deref().unwrap_or_else(|| {
                panic!(
                    "composite signature type `{}` has no source interface",
                    named.name
                )
            });
            let pkg = crate::world_registry::fq_name_package(source);
            ctx.type_idx(&format!("{pkg}-error-code"))
        }
        other => panic!("unsupported composite signature type: {other:?}"),
    }
}

/// Import a function-bearing interface whose signatures reference the resources
/// and error composite of a resource-defining interface (e.g. `wasi:http/client`,
/// whose `send` is `own<request> -> result<own<response>, error-code>`). Each
/// param/result type is resolved to a component-level type the resource-defining
/// pass already emitted, then outer-aliased into this interface's instance.
fn import_resource_using_composite_interface(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    iface_fq: &str,
) {
    let iface = project
        .cm_interface_registry
        .interfaces()
        .find(|i| i.path == iface_fq)
        .expect("composite resource-using interface not found in registry");
    let funcs: Vec<CmFunctionInfo> = iface
        .functions
        .iter()
        .filter(|f| {
            project.cm_interface_registry.is_function_supported(f)
                && project
                    .used_wasi_functions
                    .contains(&format!("{}::{}", f.interface_name, f.method_name))
        })
        .cloned()
        .collect();
    if funcs.is_empty() {
        return;
    }

    struct ResolvedSig {
        wasi_name: String,
        is_async: bool,
        params: Vec<(String, u32)>,
        result: Option<u32>,
    }
    let sigs: Vec<ResolvedSig> = funcs
        .iter()
        .map(|f| {
            let params = f
                .params
                .iter()
                .map(|(_, cm_name, ty)| {
                    let resolved = project.cm_interface_registry.resolve_type(ty);
                    (
                        cm_name.clone(),
                        component_type_idx_for_signature_type(builder, ctx, project, &resolved),
                    )
                })
                .collect();
            let result = f.return_type.as_ref().map(|ty| {
                let resolved = project.cm_interface_registry.resolve_type(ty);
                component_type_idx_for_signature_type(builder, ctx, project, &resolved)
            });
            ResolvedSig {
                wasi_name: f.wasi_func_name.clone(),
                is_async: f.is_async,
                params,
                result,
            }
        })
        .collect();

    let instance_type_name = format!("{}-{}-instance-type", iface.package, iface.interface);
    let instance_type_idx = ctx.register_type(&instance_type_name);
    {
        let (_, enc) = builder.ty(Some(&instance_type_name));
        let mut instance_type = InstanceType::new();
        let mut alias_local: IndexMap<u32, u32> = IndexMap::default();
        let mut next_local = 0u32;
        let mut alias = |it: &mut InstanceType, comp_idx: u32| -> u32 {
            if let Some(&l) = alias_local.get(&comp_idx) {
                return l;
            }
            it.alias(Alias::Outer {
                kind: ComponentOuterAliasKind::Type,
                count: 1,
                index: comp_idx,
            });
            let l = next_local;
            next_local += 1;
            alias_local.insert(comp_idx, l);
            l
        };
        for sig in &sigs {
            for (_, comp_idx) in &sig.params {
                alias(&mut instance_type, *comp_idx);
            }
            if let Some(comp_idx) = sig.result {
                alias(&mut instance_type, comp_idx);
            }
        }

        let mut deferred: Vec<(String, u32)> = Vec::new();
        for sig in &sigs {
            let params: Vec<(&str, ComponentValType)> = sig
                .params
                .iter()
                .map(|(n, ci)| (n.as_str(), ComponentValType::Type(alias_local[ci])))
                .collect();
            let result = sig
                .result
                .map(|ci| ComponentValType::Type(alias_local[&ci]));
            let mut fe = instance_type.ty().function();
            if sig.is_async {
                fe.async_(true).params(params).result(result);
            } else {
                fe.params(params).result(result);
            }
            let func_type_local = next_local;
            next_local += 1;
            deferred.push((sig.wasi_name.clone(), func_type_local));
        }
        for (name, idx) in &deferred {
            instance_type.export(name, wasm_encoder::ComponentTypeRef::Func(*idx));
        }
        enc.instance(&instance_type);
    }

    let instance_key = format!("{}-{}", iface.package, iface.interface);
    ctx.register_instance(&instance_key);
    builder.import(
        iface_fq,
        wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
    );

    for f in &funcs {
        let local_name = f.local_alias_name();
        ctx.register_comp_func(&local_name);
        builder.alias_export(
            ctx.instance_idx(&instance_key),
            &f.wasi_func_name,
            ComponentExportKind::Func,
        );
    }
}

fn import_interface_with_resource(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    interface_info: &crate::component_model::CmInterfaceInfo,
) {
    let Some((_resource_wado_name, resource_cm_name)) = &interface_info.resource_type else {
        return;
    };

    let Some(func) = interface_info.functions.first() else {
        return;
    };

    let local_name = func.local_alias_name();

    // Membership is decided by the plan (`ResourceGetter`) at the call site; this
    // guard is only idempotency (the getter was already emitted in this run).
    if ctx.has_comp_func(&local_name) {
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

    ctx.register_instance(&format!(
        "{}-{}",
        interface_info.package, interface_info.interface
    ));
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
            ctx.instance_idx(&format!(
                "{}-{}",
                interface_info.package, interface_info.interface
            )),
            resource_cm_name,
            ComponentExportKind::Type,
        );
    }

    ctx.register_comp_func(&local_name);
    builder.alias_export(
        ctx.instance_idx(&format!(
            "{}-{}",
            interface_info.package, interface_info.interface
        )),
        &func.wasi_func_name,
        ComponentExportKind::Func,
    );
}

/// Import a resource-defining source interface once and alias its resource
/// type(s) — and its `error-code`, for transmission futures — into the outer
/// component scope. Idempotent on `source_path` (keyed on the package-qualified
/// instance-type name, a real builder type), so repeated requests from different
/// consumers collapse to a single import.
///
/// This is the single place that emits a minimal, methods-less source instance.
/// Both the plan-driven resource-source phase and the resource-using phase's
/// pre-import call it, so the source-instance shape and its outer aliases live
/// in exactly one place rather than two divergent copies.
fn import_resource_source(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    source_path: &str,
) {
    let Some(cm_import) = crate::ast::CmImport::parse(source_path) else {
        return;
    };

    // Package-qualified names avoid collisions between same-named interfaces from
    // different packages (e.g. `wasi:cli/types` vs `wasi:filesystem/types`, both
    // interface `types`). The instance-type name doubles as the idempotency key:
    // it is a real builder type, so reusing it never desyncs the ctx/builder
    // type-index counters the way a phantom marker type would.
    let instance_name = format!("{}-{}", cm_import.package, cm_import.interface);
    let instance_type_name = format!("{instance_name}-instance-type");
    if ctx.has_type(&instance_type_name) {
        return;
    }

    // The resources this interface defines (usually exactly one).
    let resources: Vec<(String, String)> = project
        .cm_interface_registry
        .resources_for_interface(source_path)
        .map(|(name, cm_name)| (name.to_string(), cm_name.to_string()))
        .collect();
    if resources.is_empty() {
        return;
    }

    // Does this source define its own ErrorCode (needed by transmission futures)?
    let source_has_error_code = project
        .cm_interface_registry
        .variants_for_interface(source_path)
        .any(|(name, _, _)| name == "ErrorCode")
        || project
            .cm_interface_registry
            .has_enum_in_interface(source_path, "ErrorCode");

    let instance_type_idx = ctx.register_type(&instance_type_name);
    let mut local_type_idx = 0u32;
    {
        let (_, enc) = builder.ty(Some(&instance_type_name));
        let mut instance_type = InstanceType::new();
        for (_, cm_name) in &resources {
            instance_type.export(
                cm_name,
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );
            local_type_idx += 1;
        }

        // Include error-code export so it can be aliased at outer scope for
        // Transmission future types.
        if source_has_error_code {
            let interface_variants: Vec<_> = project
                .cm_interface_registry
                .variants_for_interface(source_path)
                .filter(|(name, _, _)| *name == "ErrorCode")
                .collect();
            if let Some((_, cm_name, cases)) = interface_variants.first() {
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
                    *cm_name,
                    wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(variant_idx)),
                );
            }
        }
        enc.instance(&instance_type);
    }

    ctx.register_instance(&instance_name);
    builder.import(
        source_path,
        wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
    );

    for (_, cm_name) in &resources {
        let resource_type_name = format!("resource:{cm_name}");
        if !ctx.has_type(&resource_type_name) {
            ctx.register_type(&resource_type_name);
            builder.alias_export(
                ctx.instance_idx(&instance_name),
                cm_name,
                ComponentExportKind::Type,
            );
        }
    }

    if source_has_error_code {
        let error_code_key = format!("{}-error-code", cm_import.package);
        if !ctx.has_type(&error_code_key) {
            ctx.register_type(&error_code_key);
            builder.alias_export(
                ctx.instance_idx(&instance_name),
                "error-code",
                ComponentExportKind::Type,
            );
        }
    }
}

fn import_interfaces_with_resources(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    import_plan: &[crate::wir::ImportEntry],
) {
    use crate::wir::ImportKind;
    let interfaces_with_resources: Vec<_> = project
        .cm_interface_registry
        .interfaces()
        .filter(|info| info.resource_type.is_some())
        .collect();

    // Phase 1: Import resource-defining source interfaces the plan calls for.
    // Iterate the getter interfaces (for a stable import order) and import each
    // referenced source via the shared `import_resource_source` helper, which is
    // idempotent and also handles the outer resource / error-code aliases.
    // Membership is the plan's: a source is imported iff listed as
    // `ResourceSource` (the plan decides that from `has_interface`).
    for interface_info in &interfaces_with_resources {
        let Some((resource_wado_name, _resource_cm_name)) = &interface_info.resource_type else {
            continue;
        };
        let Some(source_path) = project
            .cm_interface_registry
            .get_resource_source_interface(resource_wado_name)
        else {
            continue;
        };
        if source_path == interface_info.path {
            continue;
        }
        if !import_plan
            .iter()
            .any(|e| e.fq == source_path && e.kind == ImportKind::ResourceSource)
        {
            continue;
        }
        import_resource_source(builder, ctx, project, source_path);
    }

    // Phase 2: Import the resource-getter interfaces the plan lists
    // (`ResourceGetter`). Membership is the plan's — codegen no longer re-derives
    // it from the program's `with` set.
    for interface_info in &interfaces_with_resources {
        if !import_plan
            .iter()
            .any(|e| e.fq == interface_info.path && e.kind == ImportKind::ResourceGetter)
        {
            continue;
        }
        import_interface_with_resource(builder, ctx, interface_info);
    }

    // Register per-package error-code aliases for resource-defining interfaces.
    // These are needed by Transmission future types (future<result<_, error-code>>).
    for interface_info in &interfaces_with_resources {
        let instance_key = interface_info.instance_key();
        // Skip interfaces with no instance from the generic phases above — one
        // handled by a dedicated path (`wasi:http/types`) aliases its own
        // error-code later, and must not be mis-aliased from here.
        if !ctx.has_instance(&instance_key) {
            continue;
        }
        let has_error_code = project
            .cm_interface_registry
            .has_enum_in_interface(&interface_info.path, "ErrorCode")
            || project
                .cm_interface_registry
                .variants_for_interface(&interface_info.path)
                .any(|(name, _, _)| name == "ErrorCode");
        if has_error_code {
            let error_code_key = format!("{}-error-code", interface_info.package);
            if !ctx.has_type(&error_code_key) {
                ctx.register_type(&error_code_key);
                builder.alias_export(
                    ctx.instance_idx(&instance_key),
                    "error-code",
                    ComponentExportKind::Type,
                );
            }
        }
    }

    // Composite resource-using interfaces (e.g. the HTTP client) whose
    // signatures reference a resource-defining interface's resources/composite.
    // Emitted before the generic resource-using phase, which then self-skips them
    // (their funcs are already registered).
    for fq in import_plan
        .iter()
        .filter(|e| e.kind == ImportKind::ResourceUsingInterface)
        .filter(|e| resource_using_references_defining_interface(project, import_plan, &e.fq))
        .map(|e| e.fq.clone())
        .collect::<Vec<_>>()
    {
        import_resource_using_composite_interface(builder, ctx, project, &fq);
    }

    // Phase 3: Import interfaces that reference resources from other interfaces
    // (e.g. wasi:filesystem/preopens whose get-directories returns a list of descriptors
    // from wasi:filesystem/types). These must be imported AFTER Phase 1 so that the
    // resource outer-aliases are available in ctx.
    import_resource_using_interfaces(builder, ctx, project, import_plan);
}

/// Whether a resource-using interface's used signatures reference resources
/// defined by a `ResourceDefiningInterface` (rather than a plain resource
/// source). Such interfaces consume that interface's `{pkg}-*` own-handles and
/// error composite, so they go through `import_resource_using_composite_interface`.
fn resource_using_references_defining_interface(
    project: &NirPackage,
    import_plan: &[crate::wir::ImportEntry],
    iface_fq: &str,
) -> bool {
    let registry = project.cm_interface_registry;
    let Some(iface) = registry.interfaces().find(|i| i.path == iface_fq) else {
        return false;
    };
    let mut resources: Vec<String> = Vec::new();
    for func in &iface.functions {
        let key = format!("{}::{}", func.interface_name, func.method_name);
        if !project.used_wasi_functions.contains(&key) || !registry.is_function_supported(func) {
            continue;
        }
        if let Some(ret) = &func.return_type {
            collect_resources_in_type(ret, registry, &mut resources);
        }
        for (_, _, ty) in &func.params {
            collect_resources_in_type(ty, registry, &mut resources);
        }
    }
    resources.iter().any(|r| {
        registry
            .get_resource_source_interface(r)
            .is_some_and(|src| {
                import_plan.iter().any(|e| {
                    e.fq == src && e.kind == crate::wir::ImportKind::ResourceDefiningInterface
                })
            })
    })
}

/// Import interfaces that reference resources from other interfaces but don't define resources
/// themselves (e.g., wasi:filesystem/preopens which uses `descriptor` from wasi:filesystem/types).
///
/// Must run after Phase 1 of `import_interfaces_with_resources` so that `resource:*` types
/// have been registered in `ctx`.
fn import_resource_using_interfaces(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    import_plan: &[crate::wir::ImportEntry],
) {
    use crate::wir::ImportKind;
    for interface_info in project.cm_interface_registry.interfaces() {
        if interface_info.interface == "run" {
            continue;
        }
        if interface_info.resource_type.is_some() {
            continue;
        }
        // Membership: the plan categorizes this interface as resource-using
        // (a function-bearing interface whose signatures reference resources
        // defined elsewhere). The plan is the single source of truth; codegen
        // only encodes what the plan lists.
        if !import_plan
            .iter()
            .any(|e| e.fq == interface_info.path && e.kind == ImportKind::ResourceUsingInterface)
        {
            continue;
        }

        let supported_functions: Vec<_> = interface_info
            .functions
            .iter()
            .filter(|func| {
                if !project.cm_interface_registry.is_function_supported(func) {
                    return false;
                }
                let func_key = format!("{}::{}", func.interface_name, func.method_name);
                project.used_wasi_functions.contains(&func_key)
            })
            .collect();

        // Collect resources used in function signatures
        let mut needed_resources: Vec<String> = Vec::new();
        for func in &supported_functions {
            if let Some(ret_ty) = &func.return_type {
                collect_resources_in_type(
                    ret_ty,
                    project.cm_interface_registry,
                    &mut needed_resources,
                );
            }
            for (_, _, ty) in &func.params {
                collect_resources_in_type(ty, project.cm_interface_registry, &mut needed_resources);
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
        //
        // EXCEPTION: when the resource's source path IS this interface, we do not
        // pre-import it. The CM spec requires `[constructor]X` / `[method]X.foo`
        // to live in the same instance that exports the resource type `X` — so
        // such resources must be exported directly inside the per-interface
        // instance type emitted below, not aliased through an outer scope.
        for resource_name in &needed_resources {
            let Some(source) = project
                .cm_interface_registry
                .find_wasi_resource_source(resource_name)
            else {
                continue;
            };
            let Some(cm_name) = project
                .cm_interface_registry
                .get_resource_cm_name_by_source(source, resource_name)
            else {
                continue;
            };
            if ctx.has_type(&format!("resource:{cm_name}")) {
                continue; // already imported (by the main loop or the source phase)
            }
            let Some(source_path) = project
                .cm_interface_registry
                .get_resource_source_interface(resource_name)
            else {
                continue;
            };
            if source_path == interface_info.path.as_str() {
                // Self-owned resource — declared inline in the instance type
                // below to satisfy the constructor/method spec.
                continue;
            }
            // Pre-import the resource-defining source through the shared helper so
            // its resource type is outer-aliased before we build this instance.
            import_resource_source(builder, ctx, project, source_path);
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
                    .cm_interface_registry
                    .find_wasi_resource_source(resource_name)
                    && let Some(cm_name) = project
                        .cm_interface_registry
                        .get_resource_cm_name_by_source(source, resource_name)
                {
                    let outer_resource_type_name = format!("resource:{cm_name}");
                    let source_is_self = project
                        .cm_interface_registry
                        .get_resource_source_interface(resource_name)
                        .is_some_and(|src| src == interface_info.path.as_str());
                    if source_is_self {
                        // Declare the resource inline so [constructor]X /
                        // [method]X.foo / [static]X.foo are valid in this
                        // instance.
                        instance_type.export(
                            cm_name,
                            wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
                        );
                        resource_alias_indices.insert(resource_name.clone(), local_type_idx);
                        local_type_idx += 1;

                        let resource_local_idx = resource_alias_indices[resource_name];
                        instance_type.ty().defined_type().own(resource_local_idx);
                        own_resource_type_indices.insert(resource_name.clone(), local_type_idx);
                        local_type_idx += 1;

                        instance_type.ty().defined_type().borrow(resource_local_idx);
                        borrow_resource_type_indices.insert(resource_name.clone(), local_type_idx);
                        local_type_idx += 1;
                    } else if ctx.has_type(&outer_resource_type_name) {
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
                // Resolve params: for `self` borrow params use the borrow handle,
                // for owned-resource params use the own handle, otherwise lower
                // the type via emit_cm_val_type.
                let params: Vec<(String, ComponentValType)> = func
                    .params
                    .iter()
                    .map(|(_wado_name, cm_name, ty)| {
                        // `self: &Resource` is the canonical Wado-side form for
                        // a borrow self-param. Unwrap `Type::Reference` so the
                        // borrow handle matches both `Resource` and `&Resource`.
                        let borrow_check_ty = match ty {
                            Type::Reference(inner) => inner.as_ref(),
                            _ => ty,
                        };
                        let val = if let Type::Named(named) = borrow_check_ty
                            && let Some(&idx) = borrow_resource_type_indices.get(&named.name)
                            && cm_name == "self"
                        {
                            ComponentValType::Type(idx)
                        } else if let Type::Named(named) = ty
                            && let Some(&idx) = own_resource_type_indices.get(&named.name)
                        {
                            ComponentValType::Type(idx)
                        } else {
                            let resolved = project.cm_interface_registry.resolve_type(ty);
                            emit_cm_val_type(
                                &resolved,
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
                        };
                        (cm_name.clone(), val)
                    })
                    .collect();

                let result_type = func.return_type.as_ref().map(|ty| {
                    let resolved_ty = project.cm_interface_registry.resolve_type(ty);
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

                let param_refs: Vec<(&str, ComponentValType)> =
                    params.iter().map(|(n, t)| (n.as_str(), *t)).collect();
                let mut func_encoder = instance_type.ty().function();
                if func.is_async {
                    func_encoder
                        .async_(true)
                        .params(param_refs)
                        .result(result_type);
                } else {
                    func_encoder.params(param_refs).result(result_type);
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

        ctx.register_instance(&format!(
            "{}-{}",
            interface_info.package, interface_info.interface
        ));
        builder.import(
            &interface_info.path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        for func in &supported_functions {
            let local_name = project
                .cm_interface_registry
                .get_local_name(&interface_info.path, &func.wasi_func_name)
                .cloned()
                .unwrap_or_else(|| format!("{}-{}", interface_info.interface, func.wasi_func_name));

            ctx.register_comp_func(&local_name);
            builder.alias_export(
                ctx.instance_idx(&format!(
                    "{}-{}",
                    interface_info.package, interface_info.interface
                )),
                &func.wasi_func_name,
                ComponentExportKind::Func,
            );
        }
    }
}

fn lower_wasi_functions(
    project: &NirPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
) {
    for interface_info in project.cm_interface_registry.interfaces() {
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

            // Memory/realloc are needed when a param must be lowered through
            // linear memory or the result is returned via an outptr (more than
            // `MAX_FLAT_RESULTS` core values, e.g. a tuple or composite return).
            let returns_via_outptr = func.return_type.as_ref().is_some_and(|ty| {
                let resolved = project.cm_interface_registry.resolve_type(ty);
                crate::component_model::return_type_requires_outptr(&resolved)
                    || crate::component_model::cm_named_type_return_needs_outptr(
                        &resolved,
                        project.cm_interface_registry,
                    )
            });
            let needs_memory = func.needs_memory_with_registry(project.cm_interface_registry)
                || returns_via_outptr;
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

/// Append one CM instance export per exported interface.
///
/// World exports with `from_interface_fq` populated (CLI `run`, HTTP `handle`)
/// are grouped by that FQ — an `export Foo;` with several methods yields one
/// world export per method sharing the FQ — into a single instance carrying
/// every method's lifted func plus the union of the named CM types their
/// signatures reference. Freestanding exports (no parent interface, e.g. kiln's
/// `generate`) carry no instance; `emit_world_exports` emitted them bare.
///
/// Runs after `builder.finish()`, appending raw CM sections; the instance index
/// space continues from `ctx.instance_count()`. No HTTP-specific branching.
fn append_interface_instance_exports(
    component_bytes: &mut Vec<u8>,
    ctx: &ComponentModelContext,
    component_plan: &crate::wir_build::component_plan::ComponentPlan,
) {
    use crate::wir_build::component_plan::CmExportType;
    use wasm_encoder::{ComponentExportSection, ComponentInstanceSection, ComponentSection};

    // Collect the named CM types a boundary type references, as
    // `(cm_name, type-index)` re-export items in signature order, deduped by name.
    fn collect_type_items(
        ty: &CmExportType,
        ctx: &ComponentModelContext,
        out: &mut Vec<(String, u32)>,
    ) {
        match ty {
            CmExportType::Unit => {}
            CmExportType::Named {
                interface_fq,
                cm_name,
                is_resource,
            } => {
                if out.iter().any(|(name, _)| name == cm_name) {
                    return;
                }
                let pkg = crate::world_registry::fq_name_package(interface_fq);
                // Kind from the descriptor, not a type-registry name probe.
                let idx = if *is_resource {
                    ctx.type_idx(&format!("{pkg}-{cm_name}-resource"))
                } else {
                    ctx.type_idx(&format!("{pkg}-{cm_name}"))
                };
                out.push((cm_name.clone(), idx));
            }
            CmExportType::HandlerResult { ok, err } => {
                collect_type_items(ok, ctx, out);
                collect_type_items(err, ctx, out);
            }
        }
    }

    let mut groups: IndexMap<&str, Vec<&crate::wir_build::component_plan::WorldExportPlan>> =
        IndexMap::default();
    for export in &component_plan.world_exports {
        if let Some(fq) = &export.from_interface_fq {
            groups.entry(fq.as_str()).or_default().push(export);
        }
    }
    if groups.is_empty() {
        return;
    }

    let mut instances = ComponentInstanceSection::new();
    let mut exports = ComponentExportSection::new();
    let mut instance_idx = ctx.instance_count();

    for (&fq, group) in &groups {
        let mut type_items: Vec<(String, u32)> = Vec::new();
        for export in group {
            for (_, cm_ty) in &export.cm_params {
                collect_type_items(cm_ty, ctx, &mut type_items);
            }
            collect_type_items(&export.cm_result, ctx, &mut type_items);
        }

        let mut items: Vec<(&str, ComponentExportKind, u32)> = type_items
            .iter()
            .map(|(name, idx)| (name.as_str(), ComponentExportKind::Type, *idx))
            .collect();
        for export in group {
            items.push((
                export.name.as_str(),
                ComponentExportKind::Func,
                ctx.comp_func_idx(&export.name),
            ));
        }

        instances.export_items(items);
        exports.export(fq, ComponentExportKind::Instance, instance_idx, None);
        instance_idx += 1;
    }

    instances.append_to_component(component_bytes);
    exports.append_to_component(component_bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_cm_type_dedups_equal_keys() {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentModelContext::new();

        let leaf = ctx.register_type("leaf");
        {
            let (_, enc) = builder.ty(Some("leaf"));
            enc.defined_type().result(None, None);
        }

        let key = CmTypeKey::Result {
            ok: None,
            err: Some(Box::new(CmTypeKey::Leaf(leaf))),
        };
        let first = intern_cm_type(&mut builder, &mut ctx, &key, Some("composite"));
        let reserved_after_first = ctx.register_anon_type();
        let second = intern_cm_type(&mut builder, &mut ctx, &key, Some("composite"));

        assert_eq!(first, second, "equal keys resolve to one type index");
        assert_eq!(
            reserved_after_first,
            first + 1,
            "re-interning consumes no new type index"
        );
    }

    #[test]
    fn intern_cm_type_leaf_is_passthrough() {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentModelContext::new();
        let before = ctx.register_anon_type();
        let got = intern_cm_type(&mut builder, &mut ctx, &CmTypeKey::Leaf(7), None);
        let after = ctx.register_anon_type();
        assert_eq!(got, 7, "Leaf returns its index");
        assert_eq!(after, before + 1, "Leaf consumes no type index");
    }

    #[test]
    fn intern_cm_type_distinct_keys_distinct_indices() {
        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentModelContext::new();
        let leaf = ctx.register_type("leaf");
        {
            let (_, enc) = builder.ty(Some("leaf"));
            enc.defined_type().result(None, None);
        }
        let own = intern_cm_type(
            &mut builder,
            &mut ctx,
            &CmTypeKey::Own(Box::new(CmTypeKey::Leaf(leaf))),
            Some("own"),
        );
        let opt = intern_cm_type(
            &mut builder,
            &mut ctx,
            &CmTypeKey::Option(Box::new(CmTypeKey::Leaf(leaf))),
            Some("opt"),
        );
        assert_ne!(own, opt, "different structures get different indices");
    }
}
