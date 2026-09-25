//! Component Model generator — wraps a core module into a Wasm Component: WASI
//! interface imports and function lowering, the memory and bundled modules,
//! canonical intrinsics, core-module instantiation, and the canonical lifting
//! for world exports.

use super::component_context::{CmTypeKey, ComponentModelContext};
use crate::ast::{AstId, CmImport, NamedType, Type};
use crate::canonical::{
    CanonicalIntrinsic, CmDecl, CmDeclKind, CmFuturePayload, CmPayloadType, CmScalarType,
    CmStreamPayload,
};
use crate::codegen::emit::emit_core_module;
use crate::codegen_flags::CodegenFlags;
use crate::component_model::{
    CANONICAL_ERROR_CODE_INTERFACE, CmDefined, CmFunctionInfo, CmInterfaceInfo,
    CmInterfaceRegistry, CmTypeGen, CmTypeSink, CmVariantCase, ERROR_CODE_WADO_NAME,
    FIELDS_WADO_NAME, InstanceSink, RESPONSE_WADO_NAME, ResKind, classify_future_payload_from_ast,
    classify_stream_payload_from_ast, cm_decl_in_interface, cm_instance_key,
    cm_return_needs_outptr, emit_cm_defined, one_per_cm_name, parse_resource_func,
    wado_primitive_name_to_cm,
};
use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::kiln::import_check::KILN_TYPES_INTERFACE;
use crate::loader::WasmAsset;
use crate::nir_package::NirPackage;
use crate::synthesis::cm_binding::types::kebab_to_pascal;
use crate::test_names::{SECTION_NAME, encode};
use crate::token::Span;
use crate::wir::{ImportEntry, ImportKind, WirPackage};
use crate::wir_build::component_plan::{CmExportType, ComponentPlan, WorldExportPlan};
use crate::world_registry::fq_name_package;
use crate::{ProviderComponent, ast};
use wasm_encoder::{
    Alias, CanonicalOption, ComponentBuilder, ComponentExportKind, ComponentOuterAliasKind,
    ComponentValType, ExportKind, InstanceType, ModuleArg, PrimitiveValType, TypeBounds,
};

/// Build a complete Wasm Component from a pre-built core module and project metadata.
pub fn build_component(
    project: &NirPackage,
    core_module: &[u8],
    wir_package: &WirPackage,
    providers: &[ProviderComponent],
) -> Vec<u8> {
    let wasm_modules = &wir_package.wasm_modules;
    let mut builder = ComponentBuilder::default();
    let mut ctx = ComponentModelContext::new();

    // Generate CM imports. Which interfaces are imported, and in what category,
    // is decided by the WIR-level plan (the single source of truth); codegen
    // reads it rather than re-deriving, and its job is to encode each.
    generate_cm_imports(&mut builder, &mut ctx, project, &wir_package.import_plan);
    generate_cm_world_func_imports(&mut builder, &mut ctx, project, &wir_package.import_plan);

    // Type: result unit for run function (needed for task.return)
    let result_unit_type = ctx.register_type("result-unit");
    {
        let (_, enc) = builder.ty(Some("result-unit"));
        enc.defined_type().result(None, None);
    }

    // Emit the shared `core:kiln/types` instance before the canon and export
    // passes, which reference its `input-file`/`response`/`error` types.
    if project.is_generator_world() {
        emit_kiln_world_types(&mut builder, &mut ctx, project);
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
    let mem_module = build_memory_module(project.strip_names, mem_info);
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
        project.strip_names,
    );

    // Canonical intrinsics are discovered lazily during WIR translation via ensure_canonical().
    // They are stored in wir_package.needed_canonicals — the single source of truth.
    let all_canonical_intrinsics: Vec<CanonicalIntrinsic> =
        wir_package.needed_canonicals.iter().cloned().collect();

    // One CM type engine shared across `--lib` named-type building, so a record
    // (e.g. `point`) referenced by both a future/stream payload and an export
    // signature is defined once. The interface hint resolves lib-local named
    // types against the package's own default-interface FQ.
    let lib_iface_fq = component_plan
        .world_exports
        .iter()
        .find(|e| e.is_lib)
        .and_then(|e| e.from_interface_fq.clone());
    let mut lib_type_gen = component_plan
        .world_exports
        .iter()
        .any(|e| e.is_lib)
        .then(|| match lib_iface_fq.as_deref() {
            Some(fq) => CmTypeGen::with_interface_hint(fq),
            None => CmTypeGen::new(),
        });

    // Define named record types referenced by `Value(Named)` future/stream
    // payloads *before* the future/stream types that wrap them, so the wrapping
    // type can reference the record's index. Shares `lib_type_gen` with
    // `emit_world_exports`, so the export-signature record and the canonical
    // record are one and the same.
    prebuild_resource_payload_types(&mut builder, &mut ctx, project, &all_canonical_intrinsics);
    prebuild_value_named_types(
        &mut builder,
        &mut ctx,
        project,
        &mut lib_type_gen,
        &all_canonical_intrinsics,
    );

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
            // General (`Value`) element types intern structurally (primitives
            // inline, aggregates as defined types), keyed by the stream key.
            if let CmStreamPayload::Value(t) = &payload {
                let key = CmTypeKey::Stream(Box::new(payload_type_to_cm_key(t, &ctx, project)));
                let idx = intern_cm_type(&mut builder, &mut ctx, &key, None);
                map.insert(payload, idx);
                continue;
            }
            let (type_key, val_type) = match &payload {
                CmStreamPayload::U8 => (
                    "stream-u8".to_string(),
                    ComponentValType::Primitive(PrimitiveValType::U8),
                ),
                CmStreamPayload::Value(_) => unreachable!("handled above"),
                CmStreamPayload::Record(decl) => {
                    // An index the declaration already has, never a second one:
                    // rebinding repoints every later reader.
                    let aliased_idx = ctx.decl_type_idx(decl.def()).unwrap_or_else(|| {
                        // WASI imports are generated first, so the defining
                        // interface's instance is already available to alias from.
                        let inst_idx = ctx.instance_idx(&decl_instance_key(project, decl));
                        builder.alias_export(inst_idx, decl.cm_name(), ComponentExportKind::Type);
                        let idx = ctx.register_anon_type();
                        ctx.bind_decl_type(decl.def(), idx);
                        idx
                    });
                    (
                        format!("stream-{}", decl.name_suffix()),
                        ComponentValType::Type(aliased_idx),
                    )
                }
            };
            let idx = ctx.register_anon_type();
            let (_, enc) = builder.ty(Some(&type_key));
            enc.defined_type().stream(Some(val_type));
            map.insert(payload, idx);
        }
        map
    };

    // HTTP response types for future<T> canonical intrinsics
    let needs_trailers_future = all_canonical_intrinsics
        .iter()
        .any(|i| matches!(i.future_payload(), Some(CmFuturePayload::Trailers)));
    // The error-code declarations transmission futures carry, as the classifier
    // read them off each payload.
    let transmission_decls: IndexMap<DefId, CmDecl> = all_canonical_intrinsics
        .iter()
        .filter_map(|i| match i.future_payload() {
            Some(CmFuturePayload::Transmission(decl)) => Some((decl.def(), decl)),
            _ => None,
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

    let mut transmission_future_types: IndexMap<DefId, u32> = IndexMap::default();
    let trailers_future_type = if needs_trailers_future {
        // Asked of each candidate's own declarations: the plan's first
        // resource-defining entry need not declare the trailers type at all.
        let types_fq = wir_package
            .import_plan
            .iter()
            .filter(|e| e.kind == ImportKind::ResourceDefiningInterface)
            .map(|e| e.fq.as_str())
            .find(|fq| cm_decl_def(project, fq, FIELDS_WADO_NAME).is_some())
            .map(str::to_string)
            .expect("trailers future needs the interface declaring `Fields` in the plan");
        let defining_error_code = error_code_def(project, &types_fq).unwrap_or_else(|| {
            panic!("trailers future needs the `error-code` of `{types_fq}`, which declares none")
        });
        let (t, defining_ft) = build_future_intrinsic_types(
            &mut builder,
            &mut ctx,
            project,
            stream_u8_type,
            &types_fq,
            defining_error_code,
        );
        transmission_future_types.insert(defining_error_code, defining_ft);
        t
    } else {
        0
    };
    for (def, decl) in &transmission_decls {
        if transmission_future_types.contains_key(def) {
            continue;
        }
        let ft = build_transmission_future_type_for(&mut builder, &mut ctx, project, decl);
        transmission_future_types.insert(*def, ft);
    }

    // Build scalar future types (e.g., future<s32>) from structured metadata
    let scalar_future_types =
        build_scalar_future_types(&mut builder, &mut ctx, &all_canonical_intrinsics);

    // Build general future types (e.g., future<string>, future<list<u32>>).
    let value_future_types =
        build_value_future_types(&mut builder, &mut ctx, project, &all_canonical_intrinsics);

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
        &value_future_types,
        component_plan,
        &mut lib_type_gen,
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
    // World functions (Phase 9) lower into the same `wasi` instance.
    for (_, func) in project.cm_interface_registry.world_import_functions() {
        let local_name = func.local_alias_name();
        if ctx.has_core_func(&local_name) {
            available_wasi_funcs.insert(local_name);
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
    emit_world_exports(
        &mut builder,
        &mut ctx,
        project,
        component_plan,
        result_unit_type,
        &mut lib_type_gen,
    );

    // Test-name custom section: map each test export to its original (lossless)
    // name so `wado test` can display what the user wrote rather than the
    // ASCII-folded kebab export name. Emitted unconditionally for the test
    // world (even when empty) so the runner can rely on it being present.
    if !component_plan.test_exports.is_empty() {
        let payload = encode(
            component_plan
                .test_exports
                .iter()
                .map(|t| (t.export_name.as_str(), t.original_name.as_deref())),
        );
        builder.custom_section(&wasm_encoder::CustomSection {
            name: SECTION_NAME.into(),
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
    append_interface_instance_exports(&mut component_bytes, &ctx, project, component_plan);

    // Compose in imported CM component dependencies so the result is standalone.
    compose_dependency_components(
        component_bytes,
        project,
        &wir_package.import_plan,
        providers,
    )
}

/// Whether codegen emits `func`: the registry supports its signature and the
/// program calls it. Emitting a supported but unused function would drag
/// resource types into the instance type that the world never imports.
fn emits_function(project: &NirPackage, func: &CmFunctionInfo) -> bool {
    project.cm_interface_registry.is_function_supported(func)
        && project.used_wasi_functions.contains(&func.used_key())
}

/// The unified entry point for converting a Wado type into a `ComponentValType`,
/// defining any complex CM type — stream, future, result, list, option, tuple —
/// recursively inline in `instance_type`. A primitive or own resource is returned
/// directly, needing no new definition.
fn emit_cm_val_type(
    ty: &Type,
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    has_local_error_code: bool,
    enum_export_indices: &IndexMap<String, u32>,
    resource_handles: &IndexMap<String, ResourceTypeIndices>,
    type_gen: &mut CmTypeGen,
    project: &NirPackage,
    ctx: &mut ComponentModelContext,
) -> ComponentValType {
    let emit = |ty: &Type,
                instance_type: &mut InstanceType,
                local_type_idx: &mut u32,
                type_gen: &mut CmTypeGen,
                ctx: &mut ComponentModelContext| {
        emit_cm_val_type(
            ty,
            instance_type,
            local_type_idx,
            has_local_error_code,
            enum_export_indices,
            resource_handles,
            type_gen,
            project,
            ctx,
        )
    };
    match ty {
        Type::Generic(g) if g.name == "Stream" => {
            let element = g
                .args
                .first()
                .map(|inner| emit(inner, instance_type, local_type_idx, type_gen, ctx));
            instance_type.ty().defined_type().stream(element);
        }
        Type::Generic(g) if g.name == "Future" => {
            let payload = g
                .args
                .first()
                .map(|inner| emit(inner, instance_type, local_type_idx, type_gen, ctx));
            instance_type.ty().defined_type().future(payload);
        }
        Type::Generic(g) if g.name == "Result" => {
            let [ok, err] = g.args.as_slice() else {
                panic!("`Result` takes two type arguments, not {}", g.args.len())
            };
            // A WASI package spells `error-code` once and the others alias it,
            // so it is the one error this walk does not define in place.
            let err_type = if err.is_unit() {
                None
            } else if let Type::Named(named) = err
                && named.name == ERROR_CODE_WADO_NAME
            {
                Some(ComponentValType::Type(resolve_error_code_idx(
                    instance_type,
                    local_type_idx,
                    has_local_error_code,
                    enum_export_indices,
                    project,
                    ctx,
                )))
            } else {
                Some(emit(err, instance_type, local_type_idx, type_gen, ctx))
            };
            let ok_type = if ok.is_unit() {
                None
            } else if let Type::Named(named) = ok
                && let Some(handles) = resource_handles.get(&named.name)
            {
                Some(ComponentValType::Type(handles.own))
            } else {
                Some(shared_cm_val_type(
                    ok,
                    instance_type,
                    local_type_idx,
                    resource_handles,
                    type_gen,
                    project,
                ))
            };
            instance_type.ty().defined_type().result(ok_type, err_type);
        }
        Type::Generic(g) if g.name == "List" && !g.args.is_empty() => {
            let element = emit(&g.args[0], instance_type, local_type_idx, type_gen, ctx);
            instance_type.ty().defined_type().list(element);
        }
        Type::Generic(g) if g.name == "Option" && !g.args.is_empty() => {
            let element = emit(&g.args[0], instance_type, local_type_idx, type_gen, ctx);
            instance_type.ty().defined_type().option(element);
        }
        Type::Tuple(elems) if !elems.is_empty() => {
            let elements: Vec<ComponentValType> = elems
                .iter()
                .map(|elem| emit(elem, instance_type, local_type_idx, type_gen, ctx))
                .collect();
            instance_type.ty().defined_type().tuple(elements);
        }
        Type::Named(named) if enum_export_indices.contains_key(&named.name) => {
            return ComponentValType::Type(enum_export_indices[&named.name]);
        }
        Type::Named(named) if resource_handles.contains_key(&named.name) => {
            return ComponentValType::Type(resource_handles[&named.name].own);
        }
        _ => {
            return shared_cm_val_type(
                ty,
                instance_type,
                local_type_idx,
                resource_handles,
                type_gen,
                project,
            );
        }
    }
    let idx = *local_type_idx;
    *local_type_idx += 1;
    ComponentValType::Type(idx)
}

/// `ty` spelled by the shared generator, over this instance's resources.
fn shared_cm_val_type(
    ty: &Type,
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    resource_handles: &IndexMap<String, ResourceTypeIndices>,
    type_gen: &mut CmTypeGen,
    project: &NirPackage,
) -> ComponentValType {
    let registry = &project.cm_interface_registry;
    let resource_exports = resource_exports_for(type_gen, resource_handles, registry);
    let mut sink = InstanceSink {
        it: instance_type,
        next_idx: local_type_idx,
    };
    type_gen.ast_type_to_cm(&mut sink, ty, registry, &resource_exports)
}

/// Resolve or create the error-code type index within an instance type.
fn resolve_error_code_idx(
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    has_local_error_code: bool,
    enum_export_indices: &IndexMap<String, u32>,
    project: &NirPackage,
    ctx: &mut ComponentModelContext,
) -> u32 {
    if has_local_error_code && enum_export_indices.contains_key(ERROR_CODE_WADO_NAME) {
        enum_export_indices[ERROR_CODE_WADO_NAME]
    } else {
        let outer_ec = cm_decl_type_idx(
            ctx,
            project,
            CANONICAL_ERROR_CODE_INTERFACE,
            ERROR_CODE_WADO_NAME,
        );
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

/// The type indices one resource holds in an instance type: the resource and
/// the `own` and `borrow` handles over it.
#[derive(Clone, Copy)]
struct ResourceTypeIndices {
    resource: u32,
    own: u32,
    borrow: u32,
}

impl ResourceTypeIndices {
    /// Define the `own` and `borrow` handles over `resource`.
    fn define(instance_type: &mut InstanceType, local_type_idx: &mut u32, resource: u32) -> Self {
        instance_type.ty().defined_type().own(resource);
        let own = *local_type_idx;
        *local_type_idx += 1;
        instance_type.ty().defined_type().borrow(resource);
        let borrow = *local_type_idx;
        *local_type_idx += 1;
        Self {
            resource,
            own,
            borrow,
        }
    }
}

/// The resources under their CM names, as [`CmTypeGen`] asks for them, with
/// the handles the instance already defines registered in `type_gen`.
fn resource_exports_for<'a>(
    type_gen: &mut CmTypeGen,
    resource_handles: &IndexMap<String, ResourceTypeIndices>,
    registry: &'a CmInterfaceRegistry,
) -> IndexMap<&'a str, u32> {
    let emitting = type_gen.interface_hint().map(str::to_string);
    resource_handles
        .iter()
        .filter_map(|(wado_name, handles)| {
            let source = registry.resource_source_in(emitting.as_deref(), wado_name)?;
            let cm_name = registry.get_resource_cm_name_by_source(source, wado_name)?;
            type_gen.register_resource_handles(cm_name, handles.own, handles.borrow);
            Some((cm_name, handles.resource))
        })
        .collect()
}

/// A named type the shared generator spells as a declared CM type: a record,
/// or a local newtype kept as its alias so the boundary matches `wado wit`.
fn has_named_cm_form(ty: &Type, registry: &CmInterfaceRegistry) -> bool {
    let Type::Named(named) = ty else {
        return false;
    };
    let source = registry.source_interface(named);
    source.as_deref().is_some_and(|s| {
        registry
            .get_struct_fields_by_source(s, &named.name)
            .is_some()
    }) || registry
        .local_newtype_base(source.as_deref(), &named.name)
        .is_some()
}

fn wado_type_to_cm_val_type(
    ty: &Type,
    stream_type_idx: Option<u32>,
    enum_type_indices: &IndexMap<String, u32>,
    flags_type_indices: &IndexMap<String, u32>,
    resource_handles: &IndexMap<String, ResourceTypeIndices>,
) -> ComponentValType {
    match ty {
        Type::Named(named) => {
            if let Some(&enum_idx) = enum_type_indices.get(&named.name) {
                return ComponentValType::Type(enum_idx);
            }
            if let Some(&flags_idx) = flags_type_indices.get(&named.name) {
                return ComponentValType::Type(flags_idx);
            }
            let prim = wado_primitive_name_to_cm(&named.name)
                .unwrap_or_else(|| panic!("unsupported Wado param type for CM: {}", named.name));
            ComponentValType::Primitive(prim)
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            // borrow<resource> - WASI resource methods take self as &Resource
            if let Type::Named(named) = inner.as_ref()
                && let Some(handles) = resource_handles.get(&named.name)
            {
                return ComponentValType::Type(handles.borrow);
            }
            panic!("unsupported reference param type for CM: {ty:?}")
        }
        Type::Generic(generic) => match generic.name.as_str() {
            "Stream" => ComponentValType::Type(stream_type_idx.expect("stream type not defined")),
            _ => panic!("unsupported generic param type for CM: {}", generic.name),
        },
        _ => panic!("unsupported Wado param type for CM: {ty:?}"),
    }
}

/// Emit the memory/allocator core module. It contains no `array.copy`, so
/// codegen feature flags do not apply here.
fn build_memory_module(strip_names: bool, wasm_mod: Option<&WirPackage>) -> Vec<u8> {
    let wir = wasm_mod.expect("core:allocator with #![wasm_module(\"mem\")] is required");
    emit_core_module(wir, strip_names, CodegenFlags::default())
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
    wasm_assets: &IndexMap<String, WasmAsset>,
    strip_names: bool,
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

        let embedded = wado_wasm_embed::embed(
            &asset.bytes,
            &wado_wasm_embed::Embed {
                memory_import: Some(("env", "memory")),
                keep_export: &|name| used_exports.contains(name),
                stub_export: &|_| false,
                strip_custom_sections: strip_names,
            },
        )
        .unwrap_or_else(|e| panic!("failed to process wasm asset {namespace:?}: {e}"));

        ctx.register_core_module(&module_label);
        builder.core_module_raw(Some(&module_label), &embedded);

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

/// Build the `future<result<_, error-code>>` type for one `ErrorCode`
/// declaration. Two interfaces of one package may each declare one, and the two
/// are unrelated types.
fn build_transmission_future_type_for(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    error_code: &CmDecl,
) -> u32 {
    let error_code_idx = error_code_type_idx(ctx, project, error_code.def());
    // The declaration, not its package: two interfaces of one package each
    // declaring an `error-code` would otherwise label two types alike.
    let label = error_code.name_suffix();

    let result = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Result {
            ok: None,
            err: Some(Box::new(CmTypeKey::Leaf(error_code_idx))),
        },
        Some(&format!("{label}-transmission-result")),
    );
    intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::Future(Box::new(CmTypeKey::Leaf(result))),
        Some(&format!("{label}-transmission-future")),
    )
}

/// Alias at outer scope the resources `interface_info` declares itself, where
/// `resource.drop` resolves.
fn expose_self_owned_resources(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    interface_info: &CmInterfaceInfo,
    needed_resources: &IndexSet<(String, String)>,
) {
    for (_, resource_name) in needed_resources {
        let registry = &project.cm_interface_registry;
        let Some(cm_name) = registry
            .get_resource_cm_name_by_source(&interface_info.path, resource_name)
            .map(str::to_string)
        else {
            continue;
        };
        alias_resource_type(
            builder,
            ctx,
            project,
            &interface_info.path,
            resource_name,
            &cm_name,
            &interface_info.instance_key(),
        );
    }
}

/// Alias the resource `interface_fq` declares as `resource_name` into the outer
/// component scope, bound to its declaration.
///
/// Idempotent, and the one place a resource type reaches outer scope, so every
/// consumer finds it whichever import phase put it there.
fn alias_resource_type(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    interface_fq: &str,
    resource_name: &str,
    cm_name: &str,
    instance_key: &str,
) -> u32 {
    if let Some(idx) = aliased_type_idx(ctx, project, interface_fq, resource_name, cm_name) {
        return idx;
    }
    let idx = ctx.register_type(&resource_export_key(interface_fq, cm_name));
    if let Some(def) = cm_decl_def(project, interface_fq, resource_name) {
        ctx.bind_decl_type(def, idx);
    }
    builder.alias_export(
        ctx.instance_idx(instance_key),
        cm_name,
        ComponentExportKind::Type,
    );
    idx
}

/// The outer index an import phase gave the type `interface_fq` exports as
/// `cm_name`, or `None` where none has.
fn aliased_type_idx(
    ctx: &ComponentModelContext,
    project: &NirPackage,
    interface_fq: &str,
    wado_name: &str,
    cm_name: &str,
) -> Option<u32> {
    cm_decl_def(project, interface_fq, wado_name)
        .and_then(|def| ctx.decl_type_idx(def))
        // A caller spells `wado_name` by `PascalCase`-ing the CM name, which a
        // declaration need not use. The export coordinate answers for those.
        .or_else(|| export_alias_idx(ctx, interface_fq, cm_name))
}

/// The alias an import phase made from `interface_fq`'s export `cm_name`, for a
/// reader whose declaration is bound to no index.
fn export_alias_idx(ctx: &ComponentModelContext, interface_fq: &str, cm_name: &str) -> Option<u32> {
    let key = resource_export_key(interface_fq, cm_name);
    ctx.has_type(&key).then(|| ctx.type_idx(&key))
}

/// A coordinate of this component's import surface: one interface spells one
/// export name once, so it names the alias and never a declaration.
fn resource_export_key(interface_fq: &str, cm_name: &str) -> String {
    format!("{interface_fq}#{cm_name}")
}

/// Emit the `core:kiln/types` record/variant surface once per
/// `core:kiln/generator` component: define them inside an `InstanceType`,
/// import it as `core:kiln/types@0.1.0`, and alias each exported type into the
/// component's local index space. The instance wrap is what satisfies the
/// validator's `all_valtypes_named_in_defined` check.
fn emit_kiln_world_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
) {
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
    instance_type
        .ty()
        .defined_type()
        .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
    let content_stream_local = alloc(&mut next_idx);
    let input_file_local = emit_record(
        &mut instance_type,
        &mut next_idx,
        &[
            ("path", string_vt),
            ("content", ComponentValType::Type(content_stream_local)),
        ],
    );
    emit_export(
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

    // Reserving an index per alias keeps the component's type counter in lockstep
    // with the encoder's, so downstream registrations stay in sync.
    let mut aliased: IndexMap<&str, u32> = IndexMap::default();
    for (export_name, wado_name) in [
        ("input-file", "InputFile"),
        ("output-file", "OutputFile"),
        ("response", "Response"),
        ("error", "Error"),
    ] {
        builder.alias_export(
            ctx.instance_idx("kiln-types"),
            export_name,
            ComponentExportKind::Type,
        );
        let idx = ctx.register_anon_type();
        aliased.insert(export_name, idx);
        if let Some(def) = cm_decl_def(project, KILN_TYPES_INTERFACE, wado_name) {
            ctx.bind_decl_type(def, idx);
        }
    }

    let response_idx = aliased["response"];
    let error_idx = aliased["error"];
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

/// Resolve a [`CmTypeKey`] to a [`ComponentValType`], emitting any defined type
/// inline. Primitives (and `string`) are returned as `Primitive` without a
/// defined-type slot; everything else interns to a `Type(idx)`.
fn intern_cm_valtype(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    key: &CmTypeKey,
) -> ComponentValType {
    match key {
        CmTypeKey::Leaf(idx) => ComponentValType::Type(*idx),
        CmTypeKey::Primitive(p) => ComponentValType::Primitive(*p),
        _ => ComponentValType::Type(intern_cm_type(builder, ctx, key, None)),
    }
}

/// Intern a defined CM type by its structural [`CmTypeKey`], emitting it once.
/// `debug_name` labels the emitted type in a dump and keys nothing.
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
        CmTypeKey::Primitive(_) => {
            panic!("CmTypeKey::Primitive has no defined-type slot; use intern_cm_valtype")
        }
        CmTypeKey::Own(inner) => ResolvedCmType::Own(intern_cm_type(builder, ctx, inner, None)),
        CmTypeKey::Option(inner) => ResolvedCmType::Option(intern_cm_valtype(builder, ctx, inner)),
        CmTypeKey::Future(inner) => ResolvedCmType::Future(intern_cm_valtype(builder, ctx, inner)),
        CmTypeKey::Stream(inner) => ResolvedCmType::Stream(intern_cm_valtype(builder, ctx, inner)),
        CmTypeKey::List(inner) => ResolvedCmType::List(intern_cm_valtype(builder, ctx, inner)),
        CmTypeKey::Result { ok, err } => {
            let ok = ok.as_ref().map(|k| intern_cm_valtype(builder, ctx, k));
            let err = err.as_ref().map(|k| intern_cm_valtype(builder, ctx, k));
            ResolvedCmType::Result { ok, err }
        }
        CmTypeKey::Tuple(elems) => ResolvedCmType::Tuple(
            elems
                .iter()
                .map(|k| intern_cm_valtype(builder, ctx, k))
                .collect(),
        ),
    };

    let idx = ctx.register_anon_type();
    let (_, enc) = builder.ty(debug_name);
    match resolved {
        ResolvedCmType::Own(resource) => {
            enc.defined_type().own(resource);
        }
        ResolvedCmType::Option(inner) => {
            enc.defined_type().option(inner);
        }
        ResolvedCmType::Future(inner) => {
            enc.defined_type().future(Some(inner));
        }
        ResolvedCmType::Stream(inner) => {
            enc.defined_type().stream(Some(inner));
        }
        ResolvedCmType::List(inner) => {
            enc.defined_type().list(inner);
        }
        ResolvedCmType::Result { ok, err } => {
            enc.defined_type().result(ok, err);
        }
        ResolvedCmType::Tuple(elems) => {
            enc.defined_type().tuple(elems);
        }
    }
    ctx.intern_record(key.clone(), idx);
    idx
}

enum ResolvedCmType {
    Own(u32),
    Option(ComponentValType),
    Future(ComponentValType),
    Stream(ComponentValType),
    List(ComponentValType),
    Result {
        ok: Option<ComponentValType>,
        err: Option<ComponentValType>,
    },
    Tuple(Vec<ComponentValType>),
}

fn build_future_intrinsic_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    stream_u8_type: u32,
    types_fq: &str,
    error_code_decl: DefId,
) -> (u32, u32) {
    let pkg = fq_name_package(types_fq);
    let fields_resource_idx = cm_decl_type_idx(ctx, project, types_fq, FIELDS_WADO_NAME);
    let error_code = CmTypeKey::Leaf(error_code_type_idx(ctx, project, error_code_decl));

    let fields = intern_cm_type(
        builder,
        ctx,
        &CmTypeKey::own_of(fields_resource_idx),
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
/// Convert a [`CmPayloadType`] to a structural [`CmTypeKey`] for interning.
/// Named types resolve to the component type index already registered for the
/// world's signatures (records / variants / enums / flags).
fn payload_type_to_cm_key(
    payload: &CmPayloadType,
    ctx: &ComponentModelContext,
    project: &NirPackage,
) -> CmTypeKey {
    match payload {
        CmPayloadType::Scalar(s) => CmTypeKey::Primitive(cm_scalar_to_primitive(*s)),
        CmPayloadType::String => CmTypeKey::Primitive(PrimitiveValType::String),
        CmPayloadType::List(t) => {
            CmTypeKey::List(Box::new(payload_type_to_cm_key(t, ctx, project)))
        }
        CmPayloadType::Option(t) => {
            CmTypeKey::Option(Box::new(payload_type_to_cm_key(t, ctx, project)))
        }
        CmPayloadType::Result(ok, err) => CmTypeKey::Result {
            ok: ok
                .as_ref()
                .map(|t| Box::new(payload_type_to_cm_key(t, ctx, project))),
            err: err
                .as_ref()
                .map(|t| Box::new(payload_type_to_cm_key(t, ctx, project))),
        },
        CmPayloadType::Tuple(elems) => CmTypeKey::Tuple(
            elems
                .iter()
                .map(|t| payload_type_to_cm_key(t, ctx, project))
                .collect(),
        ),
        CmPayloadType::Named(decl) => CmTypeKey::Leaf(payload_decl_type_idx(ctx, project, decl)),
        CmPayloadType::Resource(decl) => {
            CmTypeKey::own_of(payload_decl_type_idx(ctx, project, decl))
        }
    }
}

/// The imported instance whose type exports `decl`, found from the module that
/// declares it rather than searched for by its CM name.
fn decl_instance_key(project: &NirPackage, decl: &CmDecl) -> String {
    let fq = project
        .cm_interface_registry
        .interface_declaring_cm_name(decl.module(), decl.cm_name())
        .unwrap_or_else(|| {
            panic!(
                "`{}` is declared by no imported CM interface",
                decl.name_suffix()
            )
        });
    let import = CmImport::parse(fq).unwrap_or_else(|| {
        panic!("CM interface `{fq}` has no `scheme:pkg/iface` shape");
    });
    cm_instance_key(&import.package, &import.interface)
}

/// The outer type index a canonical's declaration is bound to, or the alias its
/// declaring interface exports where nothing bound one.
fn payload_decl_type_idx(ctx: &ComponentModelContext, project: &NirPackage, decl: &CmDecl) -> u32 {
    ctx.decl_type_idx(decl.def())
        .or_else(|| {
            let fq = project
                .cm_interface_registry
                .interface_declaring_cm_name(decl.module(), decl.cm_name())?;
            export_alias_idx(ctx, fq, decl.cm_name())
        })
        .unwrap_or_else(|| unaliased(&format!("the canonical payload `{}`", decl.name_suffix())))
}

/// The distinct declarations the canonicals' payloads reach as `kind`.
fn payload_decls(
    canonical_intrinsics: &[CanonicalIntrinsic],
    kind: CmDeclKind,
) -> IndexMap<DefId, CmDecl> {
    let mut out: IndexMap<DefId, CmDecl> = IndexMap::default();
    let mut keep = |decl: &CmDecl, at: CmDeclKind| {
        if at == kind {
            out.entry(decl.def()).or_insert_with(|| decl.clone());
        }
    };
    for intrinsic in canonical_intrinsics {
        if let Some(CmFuturePayload::Value(p)) = intrinsic.future_payload() {
            p.for_each_decl(&mut keep);
        }
        if let Some(CmStreamPayload::Value(p)) = intrinsic.stream_payload() {
            p.for_each_decl(&mut keep);
        }
    }
    out
}

/// Import the interface defining every resource a payload names, so `own<r>`
/// has a type to point at. Nothing else does for a guest-created future.
fn prebuild_resource_payload_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    canonical_intrinsics: &[CanonicalIntrinsic],
) {
    for (def, decl) in payload_decls(canonical_intrinsics, CmDeclKind::Resource) {
        if ctx.has_decl_type(def) {
            continue;
        }
        let Some(source) = project
            .cm_interface_registry
            .interface_declaring_cm_name(decl.module(), decl.cm_name())
            .map(str::to_string)
        else {
            continue;
        };
        import_resource_source(builder, ctx, project, &source);
    }
}

/// Define the named types a `Value(Named)` payload references, before the
/// `future<T>` / `stream<T>` wrapping them. Going through the shared
/// `lib_type_gen` keeps the export-signature type and the canonical type one.
fn prebuild_value_named_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    lib_type_gen: &mut Option<CmTypeGen>,
    canonical_intrinsics: &[CanonicalIntrinsic],
) {
    let Some(type_gen) = lib_type_gen.as_mut() else {
        return;
    };
    let no_resources: IndexMap<&str, u32> = IndexMap::default();
    for (def, _) in payload_decls(canonical_intrinsics, CmDeclKind::Value) {
        if ctx.has_decl_type(def) {
            continue;
        }
        let wado_name = project.type_table.borrow().defs().name(def).to_string();
        let named = Type::Named(NamedType::new(
            AstId::fresh(),
            wado_name,
            Span::new(0, 0, 1, 1),
        ));
        let mut sink = TopLevelSink { builder, ctx };
        let val = type_gen.ast_type_to_cm(
            &mut sink,
            &named,
            &project.cm_interface_registry,
            &no_resources,
        );
        if let ComponentValType::Type(idx) = val {
            ctx.bind_decl_type(def, idx);
        }
    }
}

/// Build `future<T>` component types for general (`Value`) future payloads,
/// returning a payload → type-index map for `resolve_future_type`.
fn build_value_future_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    canonical_intrinsics: &[CanonicalIntrinsic],
) -> IndexMap<CmPayloadType, u32> {
    let mut payloads: Vec<CmPayloadType> = Vec::new();
    for intrinsic in canonical_intrinsics {
        if let Some(CmFuturePayload::Value(t)) = intrinsic.future_payload()
            && !payloads.contains(&t)
        {
            payloads.push(t);
        }
    }
    let mut map = IndexMap::default();
    for payload in payloads {
        let key = CmTypeKey::Future(Box::new(payload_type_to_cm_key(&payload, ctx, project)));
        let idx = intern_cm_type(builder, ctx, &key, None);
        map.insert(payload, idx);
    }
    map
}

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
    transmission_future_types: &IndexMap<DefId, u32>,
    scalar_future_types: &IndexSet<(CmScalarType, u32)>,
    value_future_types: &IndexMap<CmPayloadType, u32>,
    component_plan: &ComponentPlan,
    lib_type_gen: &mut Option<CmTypeGen>,
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
                        CanonicalOption::Async,
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
                        CanonicalOption::Async,
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
                    value_future_types,
                );
                builder.future_new(ft);
            }
            CanonicalIntrinsic::FutureWrite(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                    value_future_types,
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
                    value_future_types,
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
                    value_future_types,
                );
                builder.future_cancel_read(ft, false);
            }
            CanonicalIntrinsic::FutureCancelWrite(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                    value_future_types,
                );
                builder.future_cancel_write(ft, false);
            }
            CanonicalIntrinsic::FutureDropWritable(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                    value_future_types,
                );
                builder.future_drop_writable(ft);
            }
            CanonicalIntrinsic::FutureDropReadable(payload) => {
                let ft = resolve_future_type(
                    payload.clone(),
                    trailers_future_type,
                    transmission_future_types,
                    scalar_future_types,
                    value_future_types,
                );
                builder.future_drop_readable(ft);
            }
            CanonicalIntrinsic::TaskReturn(key) => {
                let memory_idx = ctx.memory_idx();
                let result_ty = match task_return_export(key, component_plan) {
                    None => Some(ComponentValType::Type(result_unit_type)),
                    // A `--lib` export is the only one that can declare no
                    // result, and it then delivers no value: the canon carries
                    // no type, so the core import is `(func)`.
                    Some(export) if export.is_lib && export.result_type.is_none() => None,
                    Some(export) => Some(
                        lib_task_return_valtype(export, project, builder, ctx, lib_type_gen)
                            .unwrap_or_else(|| {
                                resolve_task_return_valtype(
                                    export,
                                    project,
                                    ctx,
                                    result_unit_type,
                                    trailers_future_type,
                                    transmission_future_types,
                                    scalar_future_types,
                                    value_future_types,
                                    stream_types,
                                )
                            }),
                    ),
                };
                builder.task_return(result_ty, [CanonicalOption::Memory(memory_idx)]);
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
            CanonicalIntrinsic::ResourceDrop(decl) => {
                builder.resource_drop(payload_decl_type_idx(ctx, project, decl));
            }
        }
    }
}

/// The export a `task.return` canon is keyed by. The empty key names no export:
/// it is the shared canon of the deliveries whose result is `result<>` — a test
/// export, or a WASI export taking no params and returning unit.
fn task_return_export<'a>(
    key: &str,
    component_plan: &'a ComponentPlan,
) -> Option<&'a WorldExportPlan> {
    if key.is_empty() {
        return None;
    }
    Some(
        component_plan
            .world_exports
            .iter()
            .find(|e| e.name == key)
            .unwrap_or_else(|| panic!("`task.return` keyed by `{key}`, which is no world export")),
    )
}

/// Build the `task.return` result type for a `--lib` export whose result is a
/// plain Wado type (e.g. the kiln generator's `Result<Response, Error>`), from
/// the raw AST via the shared `lib_type_gen`. Named types land top-level and are
/// cache-shared with [`emit_world_exports`], so the canon and the export func
/// type reference the same defined types. Returns `None` for a non-lib export
/// and for a `Future`/`Stream` result, both of which
/// [`resolve_task_return_valtype`] handles.
fn lib_task_return_valtype(
    export: &WorldExportPlan,
    project: &NirPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    lib_type_gen: &mut Option<CmTypeGen>,
) -> Option<ComponentValType> {
    if !export.is_lib {
        return None;
    }
    let result_type = export.result_type.as_ref()?;
    if matches!(result_type, ast::Type::Generic(g) if g.name == "Future" || g.name == "Stream") {
        return None;
    }
    let type_gen = lib_type_gen.as_mut()?;
    let no_resources: IndexMap<&str, u32> = IndexMap::default();
    let resolved = project.cm_interface_registry.resolve_type(result_type);
    let mut sink = TopLevelSink { builder, ctx };
    Some(type_gen.ast_type_to_cm(
        &mut sink,
        &resolved,
        &project.cm_interface_registry,
        &no_resources,
    ))
}

/// Resolve the `task.return` result type for an `async` export. A `--lib`
/// `future<T>` result resolves to the interned `future<T>` type; everything
/// else (WASI handler results, unit) resolves through `cm_result`.
#[allow(clippy::too_many_arguments)]
fn resolve_task_return_valtype(
    export: &WorldExportPlan,
    project: &NirPackage,
    ctx: &ComponentModelContext,
    result_unit_type: u32,
    trailers_future_type: u32,
    transmission_future_types: &IndexMap<DefId, u32>,
    scalar_future_types: &IndexSet<(CmScalarType, u32)>,
    value_future_types: &IndexMap<CmPayloadType, u32>,
    stream_types: &IndexMap<CmStreamPayload, u32>,
) -> ComponentValType {
    if export.is_lib
        && let Some(ast::Type::Generic(g)) = &export.result_type
        && g.args.len() == 1
    {
        if g.name == "Future" {
            let payload = classify_future_payload_from_ast(
                &project.type_table.borrow(),
                &g.args[0],
                &project.cm_interface_registry,
            );
            let idx = resolve_future_type(
                payload,
                trailers_future_type,
                transmission_future_types,
                scalar_future_types,
                value_future_types,
            );
            return ComponentValType::Type(idx);
        }
        if g.name == "Stream" {
            let payload = classify_stream_payload_from_ast(
                &project.type_table.borrow(),
                &g.args[0],
                &project.cm_interface_registry,
            );
            if let Some(&idx) = stream_types.get(&payload) {
                return ComponentValType::Type(idx);
            }
        }
    }
    cm_export_type_to_valtype(ctx, project, &export.cm_result, result_unit_type)
}

/// Resolve the component-level type index for a future canonical intrinsic.
fn resolve_future_type(
    payload: CmFuturePayload,
    trailers_future_type: u32,
    transmission_future_types: &IndexMap<DefId, u32>,
    scalar_future_types: &IndexSet<(CmScalarType, u32)>,
    value_future_types: &IndexMap<CmPayloadType, u32>,
) -> u32 {
    match payload {
        CmFuturePayload::Trailers => trailers_future_type,
        CmFuturePayload::Transmission(ref decl) => *transmission_future_types
            .get(&decl.def())
            .unwrap_or_else(|| {
                panic!(
                    "no transmission future type is registered for `{}`",
                    decl.name_suffix()
                )
            }),
        CmFuturePayload::Scalar(scalar) => scalar_future_types
            .iter()
            .find(|(s, _)| *s == scalar)
            .map(|(_, idx)| *idx)
            .expect("scalar future type not registered"),
        CmFuturePayload::Value(ref t) => *value_future_types
            .get(t)
            .unwrap_or_else(|| panic!("value future type not registered for: {}", t.name_suffix())),
    }
}

/// Resolve a plan-level [`CmExportType`] to a component type index registered
/// in `ctx`.
///
/// `Named` reaches the declaration the interface names and reads the index bound
/// to it. `HandlerResult` resolves through the structural interner from its
/// `ok`/`err` arms, so it shares whatever type an import phase interned.
fn cm_export_type_to_idx(
    ctx: &ComponentModelContext,
    project: &NirPackage,
    ty: &CmExportType,
    result_unit_type: u32,
) -> u32 {
    match ty {
        CmExportType::Unit => result_unit_type,
        CmExportType::Primitive(name) => panic!(
            "primitive CM export type `{name}` has no defined-type index; \
             use cm_export_type_to_valtype for primitive boundary types"
        ),
        CmExportType::Named {
            interface_fq,
            cm_name,
            // A param or return reaches a resource as its `own<>` handle, unlike
            // the re-export in `collect_type_items`, which names the type itself.
            is_resource,
        } => {
            assert!(
                !fq_name_package(interface_fq).is_empty(),
                "world export Named CM type interface `{interface_fq}` has no `scheme:pkg/...` shape",
            );
            let want = if *is_resource {
                CmDeclKind::Resource
            } else {
                CmDeclKind::Value
            };
            exported_cm_type_idx(ctx, project, interface_fq, cm_name, want)
        }
        CmExportType::HandlerResult { ok, err } => {
            let arm = |a: &CmExportType| match a {
                CmExportType::Unit => None,
                _ => Some(Box::new(CmTypeKey::Leaf(cm_export_type_to_idx(
                    ctx,
                    project,
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

/// Resolve a [`CmExportType`] to a [`ComponentValType`] for use in an export's
/// function signature. Primitives map to `ComponentValType::Primitive`; all
/// other shapes resolve through [`cm_export_type_to_idx`] to a defined-type
/// index.
fn cm_export_type_to_valtype(
    ctx: &ComponentModelContext,
    project: &NirPackage,
    ty: &CmExportType,
    result_unit_type: u32,
) -> ComponentValType {
    match ty {
        CmExportType::Primitive(name) => cm_primitive_name_to_valtype(name),
        other => {
            ComponentValType::Type(cm_export_type_to_idx(ctx, project, other, result_unit_type))
        }
    }
}

/// Map a Wado primitive type name to its Component Model `PrimitiveValType`.
fn cm_primitive_name_to_valtype(name: &str) -> ComponentValType {
    match wado_primitive_name_to_cm(name) {
        Some(prim) => ComponentValType::Primitive(prim),
        None => panic!("unsupported CM primitive type name: {name}"),
    }
}

/// [`crate::component_model::CmTypeSink`] that emits CM defined types at the
/// component's top level (via the builder) and records each named type for the
/// `--lib` default-interface instance. Used for bare world-export value types,
/// which — unlike interface-instance types — must live in the top-level type
/// space the lifted func signatures reference.
struct TopLevelSink<'a> {
    builder: &'a mut ComponentBuilder,
    ctx: &'a mut ComponentModelContext,
}

impl CmTypeSink for TopLevelSink<'_> {
    fn define(&mut self, defined: CmDefined<'_>) -> u32 {
        // Reserve the ctx index first (mirrors `intern_cm_type`) so it matches
        // the builder's appended type index.
        let idx = self.ctx.register_anon_type();
        let (_, enc) = self.builder.ty(None);
        emit_cm_defined(enc.defined_type(), defined);
        idx
    }

    fn name(&mut self, cm_name: &str, idx: u32) -> u32 {
        // Top-level types are not aliased; the instance export names them.
        // A payload reaches this index through its declaration, which
        // `prebuild_value_named_types` binds, so no name keys it here.
        self.ctx.push_lib_export_type(cm_name.to_string(), idx);
        idx
    }
}

fn emit_world_exports(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    component_plan: &ComponentPlan,
    result_unit_type: u32,
    lib_type_gen: &mut Option<CmTypeGen>,
) {
    let no_resources: IndexMap<&str, u32> = IndexMap::default();

    for export in &component_plan.world_exports {
        // The Component Model requires kebab-case extern names. Core export
        // names allow underscores, so a `--lib` export `fn id_bool` is aliased
        // from the core module by its underscore name but exported at the
        // component boundary as `id-bool` (computed at plan time as
        // `cm_export_name`). WASI world names (`run`, `handle`, `generate`) are
        // already kebab-safe, so this equals `export.name` for them.
        let cm_name = export.cm_export_name.as_str();
        let core_name = format!("{}-core", export.name);
        let func_type_name = format!("{}-func-type", export.name);

        ctx.register_core_func(&core_name);
        builder.core_alias_export(
            Some(&core_name),
            ctx.core_instance_idx("main"),
            &export.name,
            ExportKind::Func,
        );

        let func_type = if export.is_lib {
            // `--lib` export: build the param/result CM value types (and any
            // named types they reference) top-level from the raw Wado types via
            // the shared engine, *before* the func type so their indices precede
            // it. Named types land in `ctx.lib_export_types` for the instance.
            let type_gen = lib_type_gen
                .as_mut()
                .expect("lib type-gen present for sync-lift export");
            let mut param_vals: Vec<(String, ComponentValType)> = Vec::new();
            for (pname, pty) in &export.param_types {
                let resolved = project
                    .cm_interface_registry
                    .resolve_type_preserving_local_newtypes(pty);
                let mut sink = TopLevelSink {
                    builder: &mut *builder,
                    ctx: &mut *ctx,
                };
                let val = type_gen.ast_type_to_cm(
                    &mut sink,
                    &resolved,
                    &project.cm_interface_registry,
                    &no_resources,
                );
                param_vals.push((pname.clone(), val));
            }
            let result_val = export.result_type.as_ref().map(|rty| {
                let resolved = project
                    .cm_interface_registry
                    .resolve_type_preserving_local_newtypes(rty);
                let mut sink = TopLevelSink {
                    builder: &mut *builder,
                    ctx: &mut *ctx,
                };
                type_gen.ast_type_to_cm(
                    &mut sink,
                    &resolved,
                    &project.cm_interface_registry,
                    &no_resources,
                )
            });

            let func_type = ctx.register_type(&func_type_name);
            let (_, enc) = builder.ty(Some(&func_type_name));
            let param_refs: Vec<(&str, ComponentValType)> =
                param_vals.iter().map(|(n, v)| (n.as_str(), *v)).collect();
            enc.function()
                .async_(export.is_async)
                .params(param_refs)
                .result(result_val);
            func_type
        } else {
            let func_type = ctx.register_type(&func_type_name);
            {
                let (_, enc) = builder.ty(Some(&func_type_name));

                let param_vals: Vec<(String, ComponentValType)> = export
                    .cm_params
                    .iter()
                    .map(|(name, cm_ty)| {
                        (
                            name.clone(),
                            cm_export_type_to_valtype(ctx, project, cm_ty, result_unit_type),
                        )
                    })
                    .collect();
                let param_refs: Vec<(&str, ComponentValType)> = param_vals
                    .iter()
                    .map(|(n, val)| (n.as_str(), *val))
                    .collect();
                let result_val =
                    cm_export_type_to_valtype(ctx, project, &export.cm_result, result_unit_type);
                enc.function()
                    .async_(export.is_async)
                    .params(param_refs)
                    .result(Some(result_val));
            }
            func_type
        };

        ctx.register_comp_func(cm_name);
        // `realloc` is always supplied to the canon. The Wado runtime always
        // exports a `realloc` (the chosen allocator), and wasm-tools accepts
        // the option even on canons whose lift code never calls back into it
        // (verified by running the full e2e suite, including param-free CLI
        // `run` and `Result<(), ()>` exports).
        // Library exports use a synchronous lift: the core function returns the
        // lowered value(s) directly. The WASI worlds use an async lift driven by
        // `task.return`. See `WorldExportPlan::sync_lift`.
        let mut lift_opts = Vec::new();
        if !export.sync_lift {
            lift_opts.push(CanonicalOption::Async);
        }
        lift_opts.push(CanonicalOption::Memory(ctx.memory_idx()));
        lift_opts.push(CanonicalOption::Realloc(ctx.core_func_idx("realloc")));
        if let Some(post_return) = &export.post_return_core_name {
            let alias = format!("{post_return}-core");
            ctx.register_core_func(&alias);
            builder.core_alias_export(
                Some(&alias),
                ctx.core_instance_idx("main"),
                post_return,
                ExportKind::Func,
            );
            lift_opts.push(CanonicalOption::PostReturn(ctx.core_func_idx(&alias)));
        }
        builder.lift_func(
            Some(cm_name),
            ctx.core_func_idx(&core_name),
            func_type,
            lift_opts,
        );

        // Interface exports go in an instance export (added by
        // `append_interface_instance_exports`), not bare; only freestanding world
        // functions (kiln `generate`) are exported bare here.
        if export.from_interface_fq.is_none() {
            builder.export(
                cm_name,
                ComponentExportKind::Func,
                ctx.comp_func_idx(cm_name),
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
    import_plan: &[ImportEntry],
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
            .get_enum_cm_name_by_source(&cli_types_interface, ERROR_CODE_WADO_NAME)
            .expect("ErrorCode CM name not found in wasi:cli/types");
        let error_code_variants = project
            .cm_interface_registry
            .get_enum_variants_by_source(&cli_types_interface, ERROR_CODE_WADO_NAME)
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
        ctx.record_instance_error_code("cli-types", error_code_cm_name);
        let types_import_path = format!("wasi:cli/types@{cli_version}");
        builder.import(
            &types_import_path,
            wasm_encoder::ComponentTypeRef::Instance(types_instance_type),
        );

        let error_code_idx = ctx.register_anon_type();
        if let Some(def) = error_code_def(project, &cli_types_interface) {
            ctx.bind_decl_type(def, error_code_idx);
        }
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
        // A `Component` import is emitted like a host function-interface here;
        // `compose_dependency_components` wires in the dependency afterwards.
        if !import_plan.iter().any(|e| {
            e.fq == interface_info.path
                && matches!(
                    e.kind,
                    ImportKind::FunctionInterface | ImportKind::Component
                )
        }) {
            continue;
        }

        // Component imports emit their full value-type surface via
        // `ast_type_to_cm`; WASI host interfaces use the legacy emitter whose
        // `result` always resolves to the shared `error-code`.
        let is_component_import = import_plan
            .iter()
            .any(|e| e.fq == interface_info.path && e.kind == ImportKind::Component);

        // The used bindings of this interface: one per Wado name, so two names
        // binding one CM operation appear twice. Each mints its own alias.
        let supported_functions: Vec<_> = interface_info
            .functions
            .iter()
            .filter(|func| emits_function(project, func))
            .collect();

        // The CM operations behind them, one per CM name. The instance type
        // describes the imported interface, which exports each name once.
        let cm_functions = one_per_cm_name(supported_functions.iter().copied());

        // Collect resource types referenced in any function signature. The plan
        // guarantees these resolve to resources this interface defines itself
        // (a signature touching an externally-defined resource would have been
        // categorized `ResourceUsingInterface` and handled by Phase 3 instead).
        let needed_resources = project
            .cm_interface_registry
            .resources_in_signatures(&cm_functions, Some(interface_info.path.as_str()));

        let instance_type_name = format!("{}-instance-type", interface_info.interface);
        let instance_type_idx = ctx.register_type(&instance_type_name);
        let exported_error_code: Option<String>;
        {
            let (_, enc) = builder.ty(Some(&instance_type_name));
            let mut instance_type = InstanceType::new();
            let mut local_type_idx = 0u32;

            let mut resource_handles: IndexMap<String, ResourceTypeIndices> = IndexMap::default();
            for (source, resource_name) in &needed_resources {
                if let Some(cm_name) = project
                    .cm_interface_registry
                    .get_resource_cm_name_by_source(source, resource_name)
                {
                    instance_type.export(
                        cm_name,
                        wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
                    );
                    let resource_idx = local_type_idx;
                    local_type_idx += 1;
                    resource_handles.insert(
                        resource_name.clone(),
                        ResourceTypeIndices::define(
                            &mut instance_type,
                            &mut local_type_idx,
                            resource_idx,
                        ),
                    );
                }
            }

            // Determine if this interface defines its own ErrorCode (e.g. wasi:filesystem/types,
            // wasi:sockets/types) vs. using the shared wasi:cli/types error-code via outer alias.
            // ErrorCode may be an enum (cli/types) or a variant (filesystem/types, sockets/types).
            let has_local_error_code = project
                .cm_interface_registry
                .declares_own_error_code(&interface_info.path);

            let signature_types = cm_functions.iter().flat_map(|func| {
                func.params
                    .iter()
                    .map(|(_, _, ty)| ty)
                    .chain(&func.return_type)
            });
            let mut referenced_types: Vec<String> = Vec::new();
            for ty in signature_types {
                ty.for_each(&mut |ty| {
                    if let Type::Named(named) = ty
                        && !ty.is_unit()
                        && !referenced_types.contains(&named.name)
                    {
                        referenced_types.push(named.name.clone());
                    }
                });
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
                        && (name.as_str() != ERROR_CODE_WADO_NAME || has_local_error_code)
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
                        && (name.as_str() != ERROR_CODE_WADO_NAME || has_local_error_code)
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

            for enum_name in &needed_enums {
                if let Some(variants) = project
                    .cm_interface_registry
                    .get_enum_variants_by_source(iface, enum_name)
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
                        .get_enum_cm_name_by_source(iface, enum_name)
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
            let mut shared_type_gen = CmTypeGen::with_interface_hint(&interface_info.path);
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
                                emit_cm_val_type(
                                    ty,
                                    &mut instance_type,
                                    &mut local_type_idx,
                                    has_local_error_code,
                                    &enum_export_indices,
                                    &resource_handles,
                                    &mut shared_type_gen,
                                    project,
                                    ctx,
                                )
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
                    .flags_source_in(Some(&interface_info.path), flags_name)
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

            let resource_exports = resource_exports_for(
                &mut shared_type_gen,
                &resource_handles,
                &project.cm_interface_registry,
            );

            for func in &cm_functions {
                let needs_stream_u8 = func
                    .params
                    .iter()
                    .any(|(_, _, ty)| matches!(ty, Type::Generic(g) if g.name == "Stream"));
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

                let kebab_params: Vec<(String, ComponentValType)> = func
                    .params
                    .iter()
                    .map(|(_, cm_name, ty)| {
                        let resolved_ty = project
                            .cm_interface_registry
                            .resolve_type_preserving_local_newtypes(ty);
                        let is_named =
                            has_named_cm_form(&resolved_ty, &project.cm_interface_registry);
                        // A composite (`Option<T>`, `Result<T, E>`, `List<T>`, a
                        // tuple) needs the shared generator, which spells its
                        // payloads; `wado_type_to_cm_val_type` spells only the
                        // flat shapes and the pre-defined `Stream`.
                        let is_composite = match &resolved_ty {
                            Type::Generic(g) => g.name != "Stream",
                            Type::Tuple(_) => true,
                            Type::Named(_)
                            | Type::NamespacedGeneric(_)
                            | Type::Function(_)
                            | Type::Reference(_)
                            | Type::MutReference(_)
                            | Type::TypePackSpread(..)
                            | Type::Infer(_)
                            | Type::Error(_) => false,
                        };
                        let val_type = if is_component_import || is_named || is_composite {
                            let mut sink = InstanceSink {
                                it: &mut instance_type,
                                next_idx: &mut local_type_idx,
                            };
                            shared_type_gen.ast_type_to_cm(
                                &mut sink,
                                &resolved_ty,
                                &project.cm_interface_registry,
                                &resource_exports,
                            )
                        } else {
                            wado_type_to_cm_val_type(
                                &resolved_ty,
                                stream_type_idx,
                                &enum_export_indices,
                                &flags_export_indices,
                                &resource_handles,
                            )
                        };
                        (cm_name.clone(), val_type)
                    })
                    .collect();
                let params: Vec<(&str, ComponentValType)> = kebab_params
                    .iter()
                    .map(|(name, val_type)| (name.as_str(), *val_type))
                    .collect();

                // Component imports and record returns route through
                // `ast_type_to_cm`; other WASI returns use `emit_cm_val_type`.
                let result_type = func.return_type.as_ref().map(|ty| {
                    let resolved_ty = project
                        .cm_interface_registry
                        .resolve_type_preserving_local_newtypes(ty);
                    if is_component_import
                        || has_named_cm_form(&resolved_ty, &project.cm_interface_registry)
                    {
                        let mut sink = InstanceSink {
                            it: &mut instance_type,
                            next_idx: &mut local_type_idx,
                        };
                        shared_type_gen.ast_type_to_cm(
                            &mut sink,
                            &resolved_ty,
                            &project.cm_interface_registry,
                            &resource_exports,
                        )
                    } else {
                        emit_cm_val_type(
                            &resolved_ty,
                            &mut instance_type,
                            &mut local_type_idx,
                            has_local_error_code,
                            &enum_export_indices,
                            &resource_handles,
                            &mut shared_type_gen,
                            project,
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

            // The loops above emit `ErrorCode` only where a signature reaches
            // it, and the type walk may have emitted it on demand.
            exported_error_code = project
                .cm_interface_registry
                .own_error_code_cm_name(&interface_info.path)
                .filter(|cm_name| {
                    enum_export_indices.contains_key(ERROR_CODE_WADO_NAME)
                        || shared_type_gen.exported(cm_name)
                })
                .map(str::to_string);

            enc.instance(&instance_type);
        }

        ctx.register_instance(&interface_info.instance_key());
        if let Some(cm_name) = &exported_error_code {
            ctx.record_instance_error_code(&interface_info.instance_key(), cm_name);
        }
        builder.import(
            &interface_info.path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        // Expose any resources defined in this interface at the outer component scope.
        // This allows other interfaces (e.g., wasi:filesystem/preopens which uses
        // wasi:filesystem/types::descriptor) to alias them via `alias outer`.
        for (source, resource_name) in &needed_resources {
            if let Some(cm_name) = project
                .cm_interface_registry
                .get_resource_cm_name_by_source(source, resource_name)
                .map(str::to_string)
            {
                alias_resource_type(
                    builder,
                    ctx,
                    project,
                    source,
                    resource_name,
                    &cm_name,
                    &interface_info.instance_key(),
                );
            }
        }

        alias_own_error_code(
            builder,
            ctx,
            project,
            &interface_info.path,
            &interface_info.instance_key(),
        );

        for func in &supported_functions {
            alias_interface_func(builder, ctx, &interface_info, func);
        }
    }

    // Before the resource-using phase, so the resources/composites it interns
    // are available to interfaces that consume them.
    for fq in import_plan
        .iter()
        .filter(|e| e.kind == ImportKind::ResourceDefiningInterface)
        .map(|e| e.fq.clone())
        .collect::<Vec<_>>()
    {
        import_resource_defining_interface(project, builder, ctx, &fq);
    }

    import_interfaces_with_resources(builder, ctx, project, import_plan);
}

/// [`cm_decl_in_interface`] against this package's tables — codegen's entry to
/// the one name-to-identity step the declaration-identity WEP §9 sanctions.
fn cm_decl_def(project: &NirPackage, interface_fq: &str, wado_name: &str) -> Option<DefId> {
    cm_decl_in_interface(
        &project.type_table.borrow(),
        &project.cm_interface_registry,
        interface_fq,
        wado_name,
    )
}

/// The outer type index bound to the CM type `interface_fq` declares as
/// `wado_name`.
fn cm_decl_type_idx(
    ctx: &ComponentModelContext,
    project: &NirPackage,
    interface_fq: &str,
    wado_name: &str,
) -> u32 {
    cm_decl_def(project, interface_fq, wado_name)
        .and_then(|def| ctx.decl_type_idx(def))
        .unwrap_or_else(|| unaliased(&format!("`{wado_name}` of `{interface_fq}`")))
}

/// What every consumer of an outer type says when it finds none. Only the
/// validator would otherwise report a signature reaching an unaliased type.
fn unaliased(subject: &str) -> ! {
    panic!("{subject} is reached, but no imported interface aliased it into outer scope")
}

/// The `own<r>` handle over the resource type at `resource`, interned by the
/// defining interface's import.
fn own_handle_of(ctx: &ComponentModelContext, resource: u32) -> Option<u32> {
    ctx.intern_lookup(&CmTypeKey::own_of(resource))
}

/// [`own_handle_of`] for a resource reached by its declaration.
fn own_handle_idx(ctx: &ComponentModelContext, def: DefId) -> Option<u32> {
    own_handle_of(ctx, ctx.decl_type_idx(def)?)
}

/// The declaration identity of the `error-code` `interface_fq` declares.
fn error_code_def(project: &NirPackage, interface_fq: &str) -> Option<DefId> {
    cm_decl_def(project, interface_fq, ERROR_CODE_WADO_NAME)
}

/// The outer type index for the CM type `interface_fq` exports as `cm_name`, in
/// the kind the reader wants. Its Wado name is that CM name in `PascalCase`.
fn exported_cm_type_idx(
    ctx: &ComponentModelContext,
    project: &NirPackage,
    interface_fq: &str,
    cm_name: &str,
    want: CmDeclKind,
) -> u32 {
    let wado_name = kebab_to_pascal(cm_name);
    let aliased = aliased_type_idx(ctx, project, interface_fq, &wado_name, cm_name);
    let idx = match want {
        CmDeclKind::Value => aliased,
        CmDeclKind::Resource => aliased.and_then(|idx| own_handle_of(ctx, idx)),
    };
    idx.unwrap_or_else(|| unaliased(&format!("`{cm_name}` of `{interface_fq}`")))
}

/// Alias an interface's own `error-code` into the outer component scope, where
/// transmission futures and composite results look it up. Idempotent per
/// declaration, and a no-op unless the instance's type exported one.
fn alias_own_error_code(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    interface_fq: &str,
    instance_key: &str,
) {
    let Some(cm_name) = ctx.instance_error_code(instance_key).map(str::to_string) else {
        return;
    };
    let Some(def) = error_code_def(project, interface_fq) else {
        return;
    };
    if ctx.has_decl_type(def) {
        return;
    }
    let idx = ctx.register_anon_type();
    ctx.bind_decl_type(def, idx);
    builder.alias_export(
        ctx.instance_idx(instance_key),
        &cm_name,
        ComponentExportKind::Type,
    );
}

/// The outer type index standing for `def`, which an alias must already have
/// bound.
fn error_code_type_idx(ctx: &ComponentModelContext, project: &NirPackage, def: DefId) -> u32 {
    ctx.decl_type_idx(def).unwrap_or_else(|| {
        let module = project.type_table.borrow().defs().module(def).clone();
        unaliased(&format!("the `error-code` of `{module}`"))
    })
}

/// `result<own<response>, error-code>`, both arms by declaration. `None` where an
/// arm is missing from outer scope, so this interface provides only the other.
fn handler_result_key(
    ctx: &ComponentModelContext,
    response: DefId,
    error_code: DefId,
) -> Option<CmTypeKey> {
    Some(CmTypeKey::Result {
        ok: Some(Box::new(CmTypeKey::Leaf(own_handle_idx(ctx, response)?))),
        err: Some(Box::new(CmTypeKey::Leaf(ctx.decl_type_idx(error_code)?))),
    })
}

/// Export one function into an interface's instance type, emitting on the way
/// whatever types its signature reaches.
fn export_func_in_instance_type(
    instance_type: &mut InstanceType,
    type_idx: &mut u32,
    type_gen: &mut CmTypeGen,
    project: &NirPackage,
    resource_exports: &IndexMap<&str, u32>,
    func: &CmFunctionInfo,
) {
    let registry = &project.cm_interface_registry;
    let resolved_return = func
        .return_type
        .as_ref()
        .map(|ty| registry.resolve_type(ty));

    let cm_params: Vec<(String, ComponentValType)> = func
        .params
        .iter()
        .map(|(_, cm_name, ty)| {
            let mut sink = InstanceSink {
                it: instance_type,
                next_idx: type_idx,
            };
            let cm_type = type_gen.ast_type_to_cm(&mut sink, ty, registry, resource_exports);
            (cm_name.clone(), cm_type)
        })
        .collect();

    let cm_result = resolved_return.as_ref().map(|ty| {
        let mut sink = InstanceSink {
            it: instance_type,
            next_idx: type_idx,
        };
        type_gen.ast_type_to_cm(&mut sink, ty, registry, resource_exports)
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
    let func_type_idx = *type_idx;
    *type_idx += 1;

    instance_type.export(
        &func.wasi_func_name,
        wasm_encoder::ComponentTypeRef::Func(func_type_idx),
    );
}

fn import_resource_defining_interface(
    project: &NirPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    types_fq: &str,
) {
    let pkg = fq_name_package(types_fq).to_string();
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

    let resource_names: IndexSet<&str> = http_resources
        .iter()
        .map(|(wado, _)| wado.as_str())
        .collect();

    // The interface's own functions, which belong to no resource. WASI's
    // `types` interfaces have none; an interface that both defines a resource
    // and hands one out (`wasi:webgpu/webgpu#get-gpu`) does, and they are
    // imported exactly like any other interface function.
    let plain_funcs: Vec<CmFunctionInfo> = all_funcs
        .iter()
        .filter(|f| {
            !resource_names.contains(f.interface_name.as_str()) && emits_function(project, f)
        })
        .cloned()
        .collect();

    let instance_type_name = format!("{pkg}-types-instance-type");
    let http_types_instance_type = ctx.register_type(&instance_type_name);
    let exported_error_code: Option<String>;
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
        // Type indices start after the SubResource exports; the sink shares
        // this counter with the func-type allocations below.
        let mut type_idx = http_resources.len() as u32;
        let mut type_gen = CmTypeGen::with_interface_hint(types_fq);
        let resource_exports: IndexMap<&str, u32> = http_resources
            .iter()
            .enumerate()
            .map(|(i, (_, cm_name))| (cm_name.as_str(), i as u32))
            .collect();

        // A constructor or static first, since emitting its signature emits the
        // types the methods then reference (the error-code variant and its
        // payload records).
        let is_constructor_or_static = |f: &CmFunctionInfo| {
            resource_names.contains(f.interface_name.as_str())
                && matches!(
                    parse_resource_func(&f.wasi_func_name),
                    Some((ResKind::Constructor | ResKind::Static, _, _))
                )
        };
        let is_used_method = |f: &CmFunctionInfo| {
            resource_names.contains(f.interface_name.as_str())
                && !is_constructor_or_static(f)
                && matches!(
                    parse_resource_func(&f.wasi_func_name),
                    Some((ResKind::Method | ResKind::Static, _, _))
                )
                // An unused one would reference a resource type the instance
                // never emits, such as `RequestOptions`.
                && project.used_wasi_functions.contains(&f.used_key())
        };

        let exported = all_funcs
            .iter()
            .filter(|f| is_constructor_or_static(f))
            .chain(plain_funcs.iter())
            .chain(all_funcs.iter().filter(|f| is_used_method(f)));
        for func in one_per_cm_name(exported) {
            export_func_in_instance_type(
                &mut instance_type,
                &mut type_idx,
                &mut type_gen,
                project,
                &resource_exports,
                func,
            );
        }

        // `ErrorCode` lands in the instance only where a signature above
        // reached it, so the walk — not the registry — says whether it is there.
        exported_error_code = project
            .cm_interface_registry
            .own_error_code_cm_name(types_fq)
            .filter(|cm_name| type_gen.exported(cm_name))
            .map(str::to_string);

        enc.instance(&instance_type);
    }

    let types_instance = format!("{pkg}-types");
    ctx.register_instance(&types_instance);
    if let Some(cm_name) = &exported_error_code {
        ctx.record_instance_error_code(&types_instance, cm_name);
    }
    builder.import(
        types_fq,
        wasm_encoder::ComponentTypeRef::Instance(http_types_instance_type),
    );

    let mut resource_type_indices: Vec<u32> = Vec::with_capacity(http_resources.len());
    for (wado_name, cm_name) in &http_resources {
        resource_type_indices.push(alias_resource_type(
            builder,
            ctx,
            project,
            types_fq,
            wado_name,
            cm_name,
            &types_instance,
        ));
    }
    alias_own_error_code(builder, ctx, project, types_fq, &types_instance);

    // Used constructors/methods/statics and the interface's own functions,
    // aliased under their local names and lowered generically by
    // `lower_wasi_functions` — no per-constructor case.
    {
        let used_funcs: Vec<(String, String)> = all_funcs
            .iter()
            .filter(|f| {
                if !resource_names.contains(f.interface_name.as_str()) {
                    return false;
                }
                parse_resource_func(&f.wasi_func_name).is_some() && emits_function(project, f)
            })
            .chain(plain_funcs.iter())
            .map(|f| (f.wasi_func_name.clone(), f.local_alias_name()))
            .collect();
        for (cm_name, local_name) in &used_funcs {
            ctx.register_comp_func(local_name);
            builder.alias_export(
                ctx.instance_idx(&types_instance),
                cm_name,
                ComponentExportKind::Func,
            );
        }
    }

    for ((_, cm_name), resource_idx) in http_resources.iter().zip(&resource_type_indices) {
        intern_cm_type(
            builder,
            ctx,
            &CmTypeKey::own_of(*resource_idx),
            Some(&format!("{pkg}-{cm_name}")),
        );
    }

    // Intern the `result<own<response>, error-code>` handler composite when this
    // interface provides both arms (the handler/world-export shape). The export
    // lift and any client interface resolve it by structure.
    if let Some(error_code) = error_code_def(project, types_fq)
        && let Some(response) = cm_decl_def(project, types_fq, RESPONSE_WADO_NAME)
        && let Some(key) = handler_result_key(ctx, response, error_code)
    {
        intern_cm_type(builder, ctx, &key, None);
    }
}

/// Resolve a function signature type to a component-level type index, reusing
/// what a resource-defining interface already emitted: the own-handles and
/// error-code its declarations key, and the interned `result<...>`.
///
/// For a composite interface such as the HTTP client, whose signatures reference
/// another interface's resources and error composite.
fn component_type_idx_for_signature_type(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    emitting: &str,
    ty: &Type,
) -> u32 {
    let registry = &project.cm_interface_registry;
    match ty {
        Type::Generic(g) if g.name == "AsyncCall" && g.args.len() == 1 => {
            component_type_idx_for_signature_type(builder, ctx, project, emitting, &g.args[0])
        }
        Type::Generic(g) if g.name == "Result" && g.args.len() == 2 => {
            let ok =
                component_type_idx_for_signature_type(builder, ctx, project, emitting, &g.args[0]);
            let err =
                component_type_idx_for_signature_type(builder, ctx, project, emitting, &g.args[1]);
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
            component_type_idx_for_signature_type(builder, ctx, project, emitting, inner)
        }
        Type::Named(named) => {
            if let Some(source) = registry
                .resource_source_in(Some(emitting), &named.name)
                .map(str::to_string)
                .filter(|source| {
                    registry
                        .get_resource_cm_name_by_source(source, &named.name)
                        .is_some()
                })
            {
                let def = cm_decl_def(project, &source, &named.name).unwrap_or_else(|| {
                    panic!(
                        "`{source}` declares no resource `{}` for a composite signature",
                        named.name
                    )
                });
                return own_handle_idx(ctx, def).unwrap_or_else(|| {
                    unaliased(&format!("the `own<>` handle of `{}`", named.name))
                });
            }
            // A non-resource named type (e.g. the error composite's `error-code`
            // variant) resolves to the component type the resource-defining pass
            // aliased to the outer scope, which fails loudly if it was not
            // exposed there.
            let source = registry.source_interface(named).unwrap_or_else(|| {
                panic!(
                    "composite signature type `{}` has no source interface",
                    named.name
                )
            });
            cm_decl_type_idx(ctx, project, &source, &named.name)
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
        .filter(|f| emits_function(project, f))
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
                        component_type_idx_for_signature_type(
                            builder, ctx, project, iface_fq, &resolved,
                        ),
                    )
                })
                .collect();
            let result = f.return_type.as_ref().map(|ty| {
                let resolved = project.cm_interface_registry.resolve_type(ty);
                component_type_idx_for_signature_type(builder, ctx, project, iface_fq, &resolved)
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

    let instance_key = iface.instance_key();
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
    project: &NirPackage,
    interface_info: &CmInterfaceInfo,
) {
    let Some((resource_wado_name, resource_cm_name)) = &interface_info.resource_type else {
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

    // The getter's resource may be declared by another interface, so the
    // declaring one answers which declaration this is.
    let declaring = project
        .cm_interface_registry
        .resource_source_in(Some(&interface_info.path), resource_wado_name)
        .map(str::to_string);
    let outer_resource_idx = declaring.as_deref().and_then(|source| {
        aliased_type_idx(ctx, project, source, resource_wado_name, resource_cm_name)
    });

    let instance_type_name = format!("{}-instance-type", interface_info.interface);
    let instance_type_idx = ctx.register_type(&instance_type_name);
    {
        let (_, enc) = builder.ty(Some(&instance_type_name));
        let mut instance_type = InstanceType::new();

        if let Some(outer_type_idx) = outer_resource_idx {
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

    ctx.register_instance(&interface_info.instance_key());
    builder.import(
        &interface_info.path,
        wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
    );

    // Expose the resource at outer scope, keyed by the interface that declares
    // it: that is the coordinate every other reader asks for.
    if outer_resource_idx.is_none() {
        alias_resource_type(
            builder,
            ctx,
            project,
            declaring.as_deref().unwrap_or(&interface_info.path),
            resource_wado_name,
            resource_cm_name,
            &interface_info.instance_key(),
        );
    }

    ctx.register_comp_func(&local_name);
    builder.alias_export(
        ctx.instance_idx(&interface_info.instance_key()),
        &func.wasi_func_name,
        ComponentExportKind::Func,
    );
}

/// Import a resource-defining source interface once and alias its resource types
/// — and its `error-code`, for transmission futures — into the outer component
/// scope. Idempotent on `source_path`, keyed by the package-qualified
/// instance-type name. The single place emitting a methods-less source instance,
/// so the plan-driven and resource-using phases cannot diverge into two copies.
fn import_resource_source(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    source_path: &str,
) {
    let Some(cm_import) = CmImport::parse(source_path) else {
        return;
    };

    // Package-qualified names avoid collisions between same-named interfaces from
    // different packages (e.g. `wasi:cli/types` vs `wasi:filesystem/types`, both
    // interface `types`). The instance-type name doubles as the idempotency key:
    // it is a real builder type, so reusing it never desyncs the ctx/builder
    // type-index counters the way a phantom marker type would.
    let instance_name = cm_instance_key(&cm_import.package, &cm_import.interface);
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

    // The source's own ErrorCode, in whichever shape it declares — transmission
    // future types alias it at outer scope.
    let error_code_cm_name = project
        .cm_interface_registry
        .own_error_code_cm_name(source_path)
        .map(str::to_string);

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

        if let Some(cm_name) = &error_code_cm_name {
            emit_error_code_export(
                &mut instance_type,
                &mut local_type_idx,
                project,
                source_path,
                cm_name,
            );
        }
        enc.instance(&instance_type);
    }

    ctx.register_instance(&instance_name);
    if let Some(cm_name) = &error_code_cm_name {
        ctx.record_instance_error_code(&instance_name, cm_name);
    }
    builder.import(
        source_path,
        wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
    );

    for (wado_name, cm_name) in &resources {
        alias_resource_type(
            builder,
            ctx,
            project,
            source_path,
            wado_name,
            cm_name,
            &instance_name,
        );
    }

    alias_own_error_code(builder, ctx, project, source_path, &instance_name);
}

/// Emit `source_path`'s own `ErrorCode` into an instance type under `cm_name`,
/// in whichever shape it declares: a variant (`wasi:filesystem/types`) or an
/// enum (`wasi:cli/types`).
fn emit_error_code_export(
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    project: &NirPackage,
    source_path: &str,
    cm_name: &str,
) {
    let registry = &project.cm_interface_registry;
    if let Some(cases) = registry
        .get_variant_cases_by_source(source_path, ERROR_CODE_WADO_NAME)
        .map(<[CmVariantCase]>::to_vec)
    {
        let mut type_gen = CmTypeGen::with_interface_hint(source_path);
        let no_resources: IndexMap<&str, u32> = IndexMap::default();
        let cm_cases: Vec<(&str, Option<ComponentValType>)> = cases
            .iter()
            .map(|case| {
                let payload = case.payload.as_ref().map(|ty| {
                    let mut sink = InstanceSink {
                        it: instance_type,
                        next_idx: local_type_idx,
                    };
                    type_gen.ast_type_to_cm(&mut sink, ty, registry, &no_resources)
                });
                (case.cm_name.as_str(), payload)
            })
            .collect();
        instance_type.ty().defined_type().variant(cm_cases);
    } else if let Some(members) =
        registry.get_enum_variants_by_source(source_path, ERROR_CODE_WADO_NAME)
    {
        instance_type
            .ty()
            .defined_type()
            .enum_type(members.iter().map(String::as_str));
    } else {
        panic!("`{source_path}` has no ErrorCode to export as `{cm_name}`");
    }
    let defined_idx = *local_type_idx;
    instance_type.export(
        cm_name,
        wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(defined_idx)),
    );
    // The definition above and this export each take an index in the instance
    // type's own space, so the caller's counter advances by two.
    *local_type_idx += 2;
}

fn import_interfaces_with_resources(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    import_plan: &[ImportEntry],
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
    // `ResourceSource`.
    for interface_info in &interfaces_with_resources {
        let Some((resource_wado_name, _resource_cm_name)) = &interface_info.resource_type else {
            continue;
        };
        let Some(source_path) = project
            .cm_interface_registry
            .resource_source_in(Some(&interface_info.path), resource_wado_name)
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
        import_interface_with_resource(builder, ctx, project, interface_info);
    }

    // Alias each resource-defining interface's own error-code, which the
    // transmission future types (`future<result<_, error-code>>`) look up.
    for interface_info in &interfaces_with_resources {
        let instance_key = interface_info.instance_key();
        // Skip interfaces with no instance from the generic phases above — one
        // handled by a dedicated path (`wasi:http/types`) aliases its own
        // error-code later, and must not be mis-aliased from here.
        if !ctx.has_instance(&instance_key) {
            continue;
        }
        alias_own_error_code(builder, ctx, project, &interface_info.path, &instance_key);
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
    import_plan: &[ImportEntry],
    iface_fq: &str,
) -> bool {
    let registry = &project.cm_interface_registry;
    let Some(iface) = registry.interfaces().find(|i| i.path == iface_fq) else {
        return false;
    };
    let emitted: Vec<&CmFunctionInfo> = iface
        .functions
        .iter()
        .filter(|func| emits_function(project, func))
        .collect();
    registry
        .resources_in_signatures(&emitted, Some(iface_fq))
        .iter()
        .any(|(source, _)| {
            import_plan
                .iter()
                .any(|e| &e.fq == source && e.kind == ImportKind::ResourceDefiningInterface)
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
    import_plan: &[ImportEntry],
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

        // A resource-using interface whose signatures reference a
        // resource-defining interface's resources/error composite (e.g. the HTTP
        // client) consumes that interface's `{pkg}-*` types by outer alias rather
        // than building them instance-locally; resolve those through the interner.
        if resource_using_references_defining_interface(project, import_plan, &interface_info.path)
        {
            import_resource_using_composite_interface(builder, ctx, project, &interface_info.path);
            continue;
        }

        let supported_functions: Vec<_> = interface_info
            .functions
            .iter()
            .filter(|func| emits_function(project, func))
            .collect();
        let cm_functions = one_per_cm_name(supported_functions.iter().copied());

        // Collect resources used in function signatures
        let needed_resources = project
            .cm_interface_registry
            .resources_in_signatures(&cm_functions, Some(interface_info.path.as_str()));

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

        // Import every needed resource at component scope before entering the
        // instance-type builder: a resource appearing only in a return type had
        // no method call to make the earlier phases import its interface. A
        // resource whose source path *is* this interface is skipped — the CM
        // spec requires `[method]X.foo` to sit in the instance exporting `X`.
        for (source, resource_name) in &needed_resources {
            let Some(cm_name) = project
                .cm_interface_registry
                .get_resource_cm_name_by_source(source, resource_name)
            else {
                continue;
            };
            if aliased_type_idx(ctx, project, source, resource_name, cm_name).is_some() {
                continue; // already imported (by the main loop or the source phase)
            }
            if source == &interface_info.path {
                // Self-owned resource — declared inline in the instance type
                // below to satisfy the constructor/method spec.
                continue;
            }
            // Pre-import the resource-defining source through the shared helper so
            // its resource type is outer-aliased before we build this instance.
            import_resource_source(builder, ctx, project, source);
        }

        // Build a map: resource_wado_name -> local_type_idx_in_instance_type
        // First alias outer resources, then build own<>/borrow<> types.
        let instance_type_name = format!("{}-instance-type", interface_info.interface);
        let instance_type_idx = ctx.register_type(&instance_type_name);
        {
            let (_, enc) = builder.ty(Some(&instance_type_name));
            let mut instance_type = InstanceType::new();
            let mut local_type_idx = 0u32;

            let mut resource_handles: IndexMap<String, ResourceTypeIndices> = IndexMap::default();

            for (source, resource_name) in &needed_resources {
                let Some(cm_name) = project
                    .cm_interface_registry
                    .get_resource_cm_name_by_source(source, resource_name)
                else {
                    continue;
                };
                if source == &interface_info.path {
                    // Declare the resource inline so [constructor]X /
                    // [method]X.foo / [static]X.foo are valid in this
                    // instance.
                    instance_type.export(
                        cm_name,
                        wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
                    );
                } else if let Some(outer_idx) =
                    aliased_type_idx(ctx, project, source, resource_name, cm_name)
                {
                    instance_type.alias(Alias::Outer {
                        kind: ComponentOuterAliasKind::Type,
                        count: 1,
                        index: outer_idx,
                    });
                } else {
                    // Resource not yet imported — skip this interface
                    continue;
                }
                let resource_idx = local_type_idx;
                local_type_idx += 1;
                resource_handles.insert(
                    resource_name.clone(),
                    ResourceTypeIndices::define(
                        &mut instance_type,
                        &mut local_type_idx,
                        resource_idx,
                    ),
                );
            }

            let mut type_gen = CmTypeGen::with_interface_hint(&interface_info.path);
            let mut deferred_func_exports: Vec<(String, u32)> = Vec::new();

            for func in &cm_functions {
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
                            && let Some(handles) = resource_handles.get(&named.name)
                            && cm_name == "self"
                        {
                            ComponentValType::Type(handles.borrow)
                        } else if let Type::Named(named) = ty
                            && let Some(handles) = resource_handles.get(&named.name)
                        {
                            ComponentValType::Type(handles.own)
                        } else {
                            let resolved = project.cm_interface_registry.resolve_type(ty);
                            emit_cm_val_type(
                                &resolved,
                                &mut instance_type,
                                &mut local_type_idx,
                                false,
                                &IndexMap::default(),
                                &resource_handles,
                                &mut type_gen,
                                project,
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
                        false,
                        &IndexMap::default(),
                        &resource_handles,
                        &mut type_gen,
                        project,
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

        ctx.register_instance(&interface_info.instance_key());
        builder.import(
            &interface_info.path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        // Declared inline in the instance type above, as the CM spec requires
        // for `[constructor]X` / `[method]X.foo`; `resource.drop` needs them at
        // outer scope too.
        expose_self_owned_resources(builder, ctx, project, &interface_info, &needed_resources);

        for func in &supported_functions {
            alias_interface_func(builder, ctx, &interface_info, func);
        }
    }
}

/// Collect a component asset down to the lifted functions the program imports.
/// An asset the pass cannot read is composed whole: the program is correct
/// either way, and only its size is at stake.
fn collect_component_asset(asset: &WasmAsset, project: &NirPackage) -> Vec<u8> {
    // The component's own items are left alone, so a lift the program does not
    // import still names a core export. That export stays as a stub, and what
    // only its body reached is what the collection takes away.
    let unused: IndexSet<String> = project
        .cm_interface_registry
        .interfaces()
        .filter(|interface| asset.component_interface_fqs.contains(&interface.path))
        .flat_map(|interface| {
            interface.functions.into_iter().filter_map(|func| {
                let used = project.used_wasi_functions.contains(&func.used_key());
                (!used).then(|| func.cm_identifier())
            })
        })
        .collect();

    // A `cabi_post_` export belongs to the lift it is named after, so it lives
    // and dies with it rather than being asked about separately.
    let stub = |name: &str| unused.contains(name.strip_prefix("cabi_post_").unwrap_or(name));
    wado_wasm_embed::embed_component(
        &asset.bytes,
        &wado_wasm_embed::Embed {
            memory_import: None,
            keep_export: &|_| true,
            stub_export: &stub,
            strip_custom_sections: true,
        },
    )
    .unwrap_or_else(|_| asset.bytes.clone())
}

/// Compose `program_bytes` with its `ImportKind::Component` dependencies into
/// one standalone component via `wasm-compose`. Each dependency's exported
/// interface is connected to the program's matching import; leftover host
/// imports are merged by name. The fused guest-to-guest call elides the CM
/// reentrancy guard (no `CannotEnterComponent` trap a host trampoline would hit).
fn compose_dependency_components(
    program_bytes: Vec<u8>,
    project: &NirPackage,
    import_plan: &[ImportEntry],
    providers: &[ProviderComponent],
) -> Vec<u8> {
    use crate::wir::ImportKind;
    use wasm_compose::graph::{
        Component, ComponentId, CompositionGraph, EncodeOptions, ImportIndex, InstanceId,
    };
    use wasmparser::Validator;

    let dependency_fqs: IndexSet<&str> = import_plan
        .iter()
        .filter(|e| e.kind == ImportKind::Component)
        .map(|e| e.fq.as_str())
        .collect();
    // World functions (Phase 9) compose by their CM (WIT) name, but the plan
    // keys them by the Wado (snake) name, so translate via the registry —
    // else a multi-word `render-html` never matches its plan entry `render_html`.
    let planned_world_funcs: IndexSet<&str> = import_plan
        .iter()
        .filter(|e| e.kind == ImportKind::WorldFunction)
        .map(|e| e.fq.as_str())
        .collect();
    let world_func_cm_names: IndexSet<String> = project
        .cm_interface_registry
        .world_import_functions()
        .filter(|(name, _)| planned_world_funcs.contains(name))
        .map(|(_, f)| f.wasi_func_name.clone())
        .collect();
    if dependency_fqs.is_empty() && world_func_cm_names.is_empty() && providers.is_empty() {
        return program_bytes;
    }

    let compose = || -> anyhow::Result<Vec<u8>> {
        let mut validator = Validator::new_with_features(wasmparser::WasmFeatures::all());
        let mut graph = CompositionGraph::new();

        let program = Component::from_bytes(&mut validator, "program", program_bytes.clone())?;
        let program_id = graph.add_component(program)?;
        let program_inst = graph.instantiate(program_id)?;

        // Each composed dependency's (component, instance), so a provider can be
        // wired into whichever dependency imports the interface it satisfies.
        let mut dep_instances: Vec<(ComponentId, InstanceId)> = Vec::new();

        // Instantiate each dependency, connecting its exports to program imports.
        for asset in project.wasm_assets.values() {
            let provides: Vec<&String> = asset
                .component_interface_fqs
                .iter()
                .filter(|fq| dependency_fqs.contains(fq.as_str()))
                .collect();
            let provides_funcs: Vec<&String> = asset
                .component_world_func_names
                .iter()
                .filter(|name| world_func_cm_names.contains(name.as_str()))
                .collect();
            if provides.is_empty() && provides_funcs.is_empty() {
                continue;
            }
            let collected = collect_component_asset(asset, project);
            let dep = Component::from_bytes(&mut validator, "dependency", collected)?;
            let dep_id = graph.add_component(dep)?;
            let dep_inst = graph.instantiate(dep_id)?;
            dep_instances.push((dep_id, dep_inst));

            for name in provides.into_iter().chain(provides_funcs) {
                let dep_export = graph
                    .get_component(dep_id)
                    .and_then(|c| c.export_by_name(name))
                    .map(|(idx, _, _)| idx)
                    .ok_or_else(|| anyhow::anyhow!("dependency missing export `{name}`"))?;
                let program_import = graph
                    .get_component(program_id)
                    .and_then(|c| c.import_by_name(name))
                    .map(|(idx, _)| idx)
                    .ok_or_else(|| anyhow::anyhow!("program missing import `{name}`"))?;
                graph.connect(dep_inst, Some(dep_export), program_inst, program_import)?;
            }
        }

        // Wire each provider's exported interface into the dependency that
        // imports it — the acyclic `provider -> dependency` shape (UseCases #8).
        for provider in providers {
            let fq = provider.import_fq.as_str();
            // No target means the dependency was dead-code-eliminated, not that
            // the name mismatched (`import_fq` comes from the dependency's own
            // imports): the provider is then unused, so skip it rather than fail.
            let targets: Vec<(InstanceId, ImportIndex)> = dep_instances
                .iter()
                .filter_map(|&(dep_id, dep_inst)| {
                    graph
                        .get_component(dep_id)
                        .and_then(|c| c.import_by_name(fq))
                        .map(|(import_idx, _)| (dep_inst, import_idx))
                })
                .collect();
            if targets.is_empty() {
                continue;
            }

            let prov = Component::from_bytes(&mut validator, "provider", provider.bytes.clone())?;
            let prov_id = graph.add_component(prov)?;
            let prov_inst = graph.instantiate(prov_id)?;
            let prov_export = graph
                .get_component(prov_id)
                .and_then(|c| c.export_by_name(fq))
                .map(|(idx, _, _)| idx)
                .ok_or_else(|| anyhow::anyhow!("provider missing export `{fq}`"))?;

            for (dep_inst, import_idx) in targets {
                graph.connect(prov_inst, Some(prov_export), dep_inst, import_idx)?;
            }
        }

        graph.encode(EncodeOptions {
            define_components: true,
            export: Some(program_inst),
            validate: false,
        })
    };

    match compose() {
        Ok(bytes) => bytes,
        // Pure transform over valid components: a failure is a compiler bug.
        Err(e) => panic!("failed to compose CM component dependencies: {e:?}"),
    }
}

/// A bare top-level `func` import per world-level function in the plan, matching
/// the dependency's world-level export. Value types go through the export side's
/// engine, so `stream<T>` and `future<T>` are defined before the func type
/// references them; a component-defined named type is rejected.
fn generate_cm_world_func_imports(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    import_plan: &[ImportEntry],
) {
    use crate::wir::ImportKind;
    let no_resources: IndexMap<&str, u32> = IndexMap::default();
    let world_funcs: Vec<CmFunctionInfo> = project
        .cm_interface_registry
        .world_import_functions()
        .map(|(_, f)| f.clone())
        .collect();
    let mut type_gen = CmTypeGen::new();
    for func in &world_funcs {
        if !import_plan
            .iter()
            .any(|e| e.kind == ImportKind::WorldFunction && e.fq == func.method_name)
        {
            continue;
        }
        let local_name = func.local_alias_name();
        let func_type_name = format!("world-func-type-{}", func.wasi_func_name);

        let mut val_type =
            |builder: &mut ComponentBuilder, ctx: &mut ComponentModelContext, ty: &Type| {
                let resolved = project
                    .cm_interface_registry
                    .resolve_type_preserving_local_newtypes(ty);
                let mut sink = TopLevelSink { builder, ctx };
                type_gen.ast_type_to_cm(
                    &mut sink,
                    &resolved,
                    &project.cm_interface_registry,
                    &no_resources,
                )
            };

        let mut param_vals: Vec<(String, ComponentValType)> = Vec::new();
        for (_, cm_name, ty) in &func.params {
            let val = val_type(builder, ctx, ty);
            param_vals.push((cm_name.clone(), val));
        }
        let result_val = func
            .return_type
            .as_ref()
            .map(|ty| val_type(builder, ctx, ty));

        let func_type = ctx.register_type(&func_type_name);
        {
            let (_, enc) = builder.ty(Some(&func_type_name));
            let param_refs: Vec<(&str, ComponentValType)> =
                param_vals.iter().map(|(n, v)| (n.as_str(), *v)).collect();
            enc.function()
                .async_(func.is_async)
                .params(param_refs)
                .result(result_val);
        }

        ctx.register_comp_func(&local_name);
        builder.import(
            &func.wasi_func_name,
            wasm_encoder::ComponentTypeRef::Func(func_type),
        );
    }
}

/// Alias `func` out of its interface instance under its own local name. Two
/// Wado names binding one CM operation are two `func`s here, so each gets its
/// own alias and the core module imports whichever one its call site named.
fn alias_interface_func(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    interface_info: &CmInterfaceInfo,
    func: &CmFunctionInfo,
) {
    ctx.register_comp_func(&func.local_alias_name());
    builder.alias_export(
        ctx.instance_idx(&interface_info.instance_key()),
        &func.wasi_func_name,
        ComponentExportKind::Func,
    );
}

fn lower_wasi_functions(
    project: &NirPackage,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
) {
    // World functions (Phase 9): canon-lower each like an interface method —
    // asynchronously when the dependency exports an `async func`, so the caller
    // drives the subtask through its `AsyncCall<T>`.
    let world_funcs: Vec<CmFunctionInfo> = project
        .cm_interface_registry
        .world_import_functions()
        .map(|(_, f)| f.clone())
        .collect();
    for func in &world_funcs {
        let local_name = func.local_alias_name();
        if !ctx.has_comp_func(&local_name) {
            continue;
        }
        ctx.register_core_func(&local_name);
        let returns_via_outptr = func.return_type.as_ref().is_some_and(|ty| {
            let resolved = project.cm_interface_registry.resolve_type(ty);
            cm_return_needs_outptr(&resolved, &project.cm_interface_registry)
        });
        let needs_memory =
            func.needs_memory_with_registry(&project.cm_interface_registry) || returns_via_outptr;
        let mut options: Vec<CanonicalOption> = Vec::new();
        if func.is_async {
            options.push(CanonicalOption::Async);
        }
        if needs_memory {
            options.push(CanonicalOption::Memory(ctx.memory_idx()));
            options.push(CanonicalOption::Realloc(ctx.core_func_idx("realloc")));
        }
        builder.lower_func(Some(&local_name), ctx.comp_func_idx(&local_name), options);
    }

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
                cm_return_needs_outptr(&resolved, &project.cm_interface_registry)
            });
            let needs_memory = func.needs_memory_with_registry(&project.cm_interface_registry)
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

/// Append one CM instance export per exported interface: world exports sharing a
/// `from_interface_fq` collapse into one instance holding every lifted func plus
/// the named CM types their signatures reference. A freestanding export has no
/// instance — `emit_world_exports` already emitted it bare. Runs after
/// `builder.finish()`, so the instance index space continues from `ctx`.
fn append_interface_instance_exports(
    component_bytes: &mut Vec<u8>,
    ctx: &ComponentModelContext,
    project: &NirPackage,
    component_plan: &ComponentPlan,
) {
    use wasm_encoder::{ComponentExportSection, ComponentInstanceSection, ComponentSection};

    // The named CM types a boundary type references, in signature order.
    fn collect_type_items(
        ty: &CmExportType,
        ctx: &ComponentModelContext,
        project: &NirPackage,
        out: &mut Vec<(String, String, u32)>,
    ) {
        match ty {
            CmExportType::Unit => {}
            // Primitives are inline value types, not named CM types — nothing
            // to re-export into an interface instance.
            CmExportType::Primitive(_) => {}
            CmExportType::Named {
                interface_fq,
                cm_name,
                is_resource: _,
            } => {
                // By owner as well as name: two interfaces may each define a
                // `request`, and the one this export defines must survive.
                if out
                    .iter()
                    .any(|(owner, name, _)| owner == interface_fq && name == cm_name)
                {
                    return;
                }
                // A re-export names the type itself, a resource's included —
                // unlike a signature, which reaches a resource's `own<>` handle.
                let idx =
                    exported_cm_type_idx(ctx, project, interface_fq, cm_name, CmDeclKind::Value);
                out.push((interface_fq.clone(), cm_name.clone(), idx));
            }
            CmExportType::HandlerResult { ok, err } => {
                collect_type_items(ok, ctx, project, out);
                collect_type_items(err, ctx, project, out);
            }
        }
    }

    let mut groups: IndexMap<&str, Vec<&WorldExportPlan>> = IndexMap::default();
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
    let base_instance_idx = ctx.instance_count();

    for (i, (&fq, group)) in groups.iter().enumerate() {
        let instance_idx = base_instance_idx + i as u32;
        let mut type_items: Vec<(String, u32)> = Vec::new();
        if group.iter().any(|e| e.is_lib) {
            // `--lib` default interface: its named types were defined top-level
            // by `emit_world_exports` and recorded on the context.
            type_items.extend(ctx.lib_export_types().iter().cloned());
        } else {
            // An interface exports the types it defines; one it `use`s stays
            // its owner's, and re-exporting it here fails a WIT decode.
            let mut named: Vec<(String, String, u32)> = Vec::new();
            for export in group {
                for (_, cm_ty) in &export.cm_params {
                    collect_type_items(cm_ty, ctx, project, &mut named);
                }
                collect_type_items(&export.cm_result, ctx, project, &mut named);
            }
            type_items.extend(
                named
                    .into_iter()
                    .filter(|(owner, _, _)| owner == fq)
                    .map(|(_, name, idx)| (name, idx)),
            );
        }

        let mut items: Vec<(&str, ComponentExportKind, u32)> = type_items
            .iter()
            .map(|(name, idx)| (name.as_str(), ComponentExportKind::Type, *idx))
            .collect();
        for export in group {
            // The instance item name and the lifted-func lookup both use the
            // kebab CM name (`emit_world_exports` registers under it); the
            // underscore `export.name` is only the core-module symbol.
            items.push((
                export.cm_export_name.as_str(),
                ComponentExportKind::Func,
                ctx.comp_func_idx(&export.cm_export_name),
            ));
        }

        instances.export_items(items);
        exports.export(fq, ComponentExportKind::Instance, instance_idx, None);
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
            &CmTypeKey::own_of(leaf),
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
