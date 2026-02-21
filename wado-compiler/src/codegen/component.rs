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
use crate::bundled::{is_fts_function, wado_bundled_fts_wasm, wado_bundled_libm_wasm};
use crate::component_model::{CmInstanceTypeGen, WasiFunctionInfo};
use crate::project::Project;
use indexmap::{IndexMap, IndexSet};
use wasm_encoder::{
    Alias, CanonicalOption, ComponentBuilder, ComponentExportKind, ComponentOuterAliasKind,
    ComponentValType, ExportKind, InstanceType, ModuleArg, PrimitiveValType, TypeBounds,
};

/// Build a complete Wasm Component from a pre-built core module and project metadata.
pub fn build_component(project: &Project, core_module: &[u8]) -> Vec<u8> {
    let mut builder = ComponentBuilder::default();
    let mut ctx = ComponentModelContext::new();

    // Generate WASI imports dynamically from registry
    generate_wasi_imports(&mut builder, &mut ctx, project);

    // Type: stream<u8> for stream intrinsics
    let stream_u8_type = ctx.register_type("stream-u8");
    {
        let (_, enc) = builder.ty(Some("stream-u8"));
        enc.defined_type()
            .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
    }

    // Type: result unit for run function (needed for task.return)
    let result_unit_type = ctx.register_type("result-unit");
    {
        let (_, enc) = builder.ty(Some("result-unit"));
        enc.defined_type().result(None, None);
    }

    // Core memory module
    let mem_module = build_memory_module(project.strip_names);
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

    // Bundled modules (FTS and libm)
    let component_plan = project
        .component_plan
        .as_ref()
        .expect("component_plan should be set by wasm_plan phase");
    let bundled_functions = &component_plan.bundled_functions;

    embed_bundled_modules(&mut builder, &mut ctx, bundled_functions);

    // HTTP response types for future<T> canonical intrinsics
    let trailers_future_type = if component_plan.needs_future_intrinsics {
        build_future_intrinsic_types(&mut builder, &mut ctx, stream_u8_type)
    } else {
        0
    };

    // Canonical intrinsics
    emit_canonical_intrinsics(
        &mut builder,
        &mut ctx,
        &component_plan.canonical_intrinsics,
        stream_u8_type,
        result_unit_type,
        trailers_future_type,
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
    let mut available_wasi_funcs: IndexSet<String> = IndexSet::new();
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
    for intrinsic_name in &component_plan.canonical_intrinsics {
        wasi_exports.push((
            intrinsic_name.clone(),
            ExportKind::Func,
            ctx.core_func_idx(intrinsic_name),
        ));
    }
    for local_name in &available_wasi_funcs {
        wasi_exports.push((
            local_name.clone(),
            ExportKind::Func,
            ctx.core_func_idx(local_name),
        ));
    }
    if project.has_http_handler_export && ctx.has_core_func("http-fields-constructor") {
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
    emit_world_exports(&mut builder, &mut ctx, component_plan, result_unit_type);

    if !project.strip_names {
        builder.append_names();
    }

    let mut component_bytes = builder.finish();

    if project.has_http_handler_export {
        append_http_handler_export(&mut component_bytes, &ctx);
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
#[allow(clippy::too_many_arguments)]
fn build_cm_tuple_types(
    elems: &[Type],
    instance_type: &mut InstanceType,
    local_type_idx: &mut u32,
    error_code_idx: Option<u32>,
    has_local_error_code: bool,
    enum_export_indices: &IndexMap<String, u32>,
    own_resource_type_indices: &IndexMap<String, u32>,
    ctx: &mut ComponentModelContext,
) -> Vec<ComponentValType> {
    let mut tuple_types = Vec::new();
    for t in elems {
        match t {
            Type::Generic(g) if g.name == "Stream" => {
                // Emit stream<u8> local type (only u8 streams are supported in WASI P3)
                instance_type
                    .ty()
                    .defined_type()
                    .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                tuple_types.push(ComponentValType::Type(*local_type_idx));
                *local_type_idx += 1;
            }
            Type::Generic(g) if g.name == "Future" => {
                // Emit future<result<_, error-code>> local type.
                // Determine the error-code type index.
                let inner_cm = if let Some(inner_g) = g.args.first()
                    && let Type::Generic(inner) = inner_g
                    && inner.name == "Result"
                {
                    let err_idx = if let Some(idx) = error_code_idx {
                        idx
                    } else if has_local_error_code && enum_export_indices.contains_key("ErrorCode")
                    {
                        enum_export_indices["ErrorCode"]
                    } else {
                        // Alias the outer error-code type.
                        let outer_ec = ctx.type_idx("error-code");
                        instance_type.alias(Alias::Outer {
                            kind: ComponentOuterAliasKind::Type,
                            count: 1,
                            index: outer_ec,
                        });
                        let idx = *local_type_idx;
                        *local_type_idx += 1;
                        idx
                    };
                    instance_type
                        .ty()
                        .defined_type()
                        .result(None, Some(ComponentValType::Type(err_idx)));
                    let result_idx = *local_type_idx;
                    *local_type_idx += 1;
                    Some(ComponentValType::Type(result_idx))
                } else {
                    None
                };
                instance_type.ty().defined_type().future(inner_cm);
                tuple_types.push(ComponentValType::Type(*local_type_idx));
                *local_type_idx += 1;
            }
            _ => {
                tuple_types.push(type_to_cm_primitive_with_resources(
                    t,
                    own_resource_type_indices,
                ));
            }
        }
    }
    tuple_types
}

/// Collect resource type names referenced anywhere in a type tree.
///
/// Used to build the `needed_resources` list for `generate_wasi_imports`.
fn collect_resources_in_type(
    ty: &Type,
    wasi_registry: &crate::component_model::WasiRegistry,
    out: &mut Vec<String>,
) {
    match ty {
        Type::Named(named) if wasi_registry.is_resource(&named.name) => {
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
    _project: &Project,
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

fn wado_type_to_cm_result_type(
    ty: &Type,
    result_type_idx: Option<u32>,
    array_type_idx: Option<u32>,
    option_type_idx: Option<u32>,
    tuple_type_idx: Option<u32>,
) -> ComponentValType {
    match ty {
        Type::Named(named) => match named.name.as_str() {
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
            _ => panic!("unsupported Wado return type for CM: {}", named.name),
        },
        Type::Generic(generic) => match generic.name.as_str() {
            "Result" => ComponentValType::Type(result_type_idx.expect("result type not defined")),
            "Array" => ComponentValType::Type(array_type_idx.expect("array type not defined")),
            "Option" => ComponentValType::Type(option_type_idx.expect("option type not defined")),
            "Tuple" => ComponentValType::Type(tuple_type_idx.expect("tuple type not defined")),
            _ => panic!("unsupported generic return type for CM: {}", generic.name),
        },
        Type::Tuple(_) => ComponentValType::Type(tuple_type_idx.expect("tuple type not defined")),
        _ => panic!("unsupported Wado return type for CM: {ty:?}"),
    }
}

fn build_memory_module(strip_names: bool) -> Vec<u8> {
    use wasm_encoder::{
        CodeSection, ExportKind, ExportSection, Function, FunctionSection, GlobalSection,
        GlobalType, Instruction, MemorySection, MemoryType, Module, NameMap, NameSection,
        TypeSection, ValType,
    };

    let mut module = Module::new();

    let mut types = TypeSection::new();
    types.ty().function(
        [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        [ValType::I32],
    );
    module.section(&types);

    let mut functions = FunctionSection::new();
    functions.function(0);
    module.section(&functions);

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 17,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&memories);

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &wasm_encoder::ConstExpr::i32_const(1024),
    );
    module.section(&globals);

    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    exports.export("realloc", ExportKind::Func, 0);
    module.section(&exports);

    let mut code = CodeSection::new();
    let mut realloc_func = Function::new([(1, ValType::I32)]);
    realloc_func.instruction(&Instruction::GlobalGet(0));
    realloc_func.instruction(&Instruction::LocalGet(2));
    realloc_func.instruction(&Instruction::I32Add);
    realloc_func.instruction(&Instruction::I32Const(1));
    realloc_func.instruction(&Instruction::I32Sub);
    realloc_func.instruction(&Instruction::I32Const(0));
    realloc_func.instruction(&Instruction::LocalGet(2));
    realloc_func.instruction(&Instruction::I32Sub);
    realloc_func.instruction(&Instruction::I32And);
    realloc_func.instruction(&Instruction::LocalSet(4));
    realloc_func.instruction(&Instruction::LocalGet(4));
    realloc_func.instruction(&Instruction::LocalGet(3));
    realloc_func.instruction(&Instruction::I32Add);
    realloc_func.instruction(&Instruction::GlobalSet(0));
    realloc_func.instruction(&Instruction::LocalGet(4));
    realloc_func.instruction(&Instruction::End);
    code.function(&realloc_func);
    module.section(&code);

    if !strip_names {
        let mut names = NameSection::new();
        let mut func_names = NameMap::new();
        func_names.append(0, "realloc");
        names.functions(&func_names);
        let mut type_names = NameMap::new();
        type_names.append(0, "realloc");
        names.types(&type_names);
        module.section(&names);
    }

    module.finish()
}

fn embed_bundled_modules(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    bundled_functions: &[String],
) {
    let fts_functions: Vec<_> = bundled_functions
        .iter()
        .filter(|f| is_fts_function(f))
        .cloned()
        .collect();
    let libm_functions: Vec<_> = bundled_functions
        .iter()
        .filter(|f| !is_fts_function(f))
        .cloned()
        .collect();

    if !fts_functions.is_empty() {
        let fts_module =
            postprocess::convert_memory_to_import(wado_bundled_fts_wasm(), "env", "memory")
                .expect("Failed to process wado-bundled-fts module");

        let keep_exports: IndexSet<_> = fts_functions.iter().cloned().collect();
        let final_module = postprocess::eliminate_dead_code(&fts_module, &keep_exports);

        ctx.register_core_module("fts-mod");
        builder.core_module_raw(Some("fts-mod"), &final_module);

        ctx.register_core_instance("fts-env");
        let fts_env_exports = [("memory", ExportKind::Memory, ctx.memory_idx())];
        let fts_env_instance =
            builder.core_instantiate_exports(Some("fts-env-instance"), fts_env_exports);

        ctx.register_core_instance("fts");
        builder.core_instantiate(
            Some("fts"),
            ctx.core_module_idx("fts-mod"),
            [("env", ModuleArg::Instance(fts_env_instance))],
        );

        for func_name in &fts_functions {
            ctx.register_core_func(func_name);
            builder.core_alias_export(
                Some(func_name),
                ctx.core_instance_idx("fts"),
                func_name,
                ExportKind::Func,
            );
        }
    }

    if !libm_functions.is_empty() {
        let libm_module =
            postprocess::convert_memory_to_import(wado_bundled_libm_wasm(), "env", "memory")
                .expect("Failed to process wado-bundled-libm module");

        let keep_exports: IndexSet<_> = libm_functions.iter().cloned().collect();
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

        for func_name in &libm_functions {
            ctx.register_core_func(func_name);
            builder.core_alias_export(
                Some(func_name),
                ctx.core_instance_idx("libm"),
                func_name,
                ExportKind::Func,
            );
        }
    }
}

fn build_future_intrinsic_types(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    stream_u8_type: u32,
) -> u32 {
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

    trailers_future_type
}

fn emit_canonical_intrinsics(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    canonical_intrinsics: &[String],
    stream_u8_type: u32,
    result_unit_type: u32,
    trailers_future_type: u32,
) {
    for name in canonical_intrinsics {
        let name = name.as_str();
        ctx.register_core_func(name);

        match name {
            "stream-new" => {
                builder.stream_new(stream_u8_type);
            }
            "stream-write" => {
                builder.stream_write(
                    stream_u8_type,
                    [
                        CanonicalOption::Memory(ctx.memory_idx()),
                        CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                    ],
                );
            }
            "stream-drop-writable" => {
                builder.stream_drop_writable(stream_u8_type);
            }
            "stream-drop-readable" => {
                builder.stream_drop_readable(stream_u8_type);
            }
            "future-new" => {
                builder.future_new(trailers_future_type);
            }
            "future-write" => {
                builder.future_write(
                    trailers_future_type,
                    [
                        CanonicalOption::Memory(ctx.memory_idx()),
                        CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                    ],
                );
            }
            "future-drop-writable" => {
                builder.future_drop_writable(trailers_future_type);
            }
            "future-drop-readable" => {
                builder.future_drop_readable(trailers_future_type);
            }
            "task-return" => {
                let task_return_type = if ctx.has_type("http-handler-result") {
                    ctx.type_idx("http-handler-result")
                } else {
                    result_unit_type
                };
                builder.task_return(
                    Some(ComponentValType::Type(task_return_type)),
                    [CanonicalOption::Memory(ctx.memory_idx())],
                );
            }
            "waitable-set-new" => {
                builder.waitable_set_new();
            }
            "waitable-join" => {
                builder.waitable_join();
            }
            "waitable-set-wait" => {
                builder.waitable_set_wait(false, ctx.memory_idx());
            }
            "subtask-drop" => {
                builder.subtask_drop();
            }
            _ => {}
        }
    }
}

fn emit_world_exports(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    component_plan: &crate::wasm_plan::ComponentPlan,
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

            if export.is_http_handler {
                let request_type_idx = ctx.type_idx("http-request");
                let handler_result_type_idx = ctx.type_idx("http-handler-result");
                enc.function()
                    .async_(export.is_async)
                    .params([("request", ComponentValType::Type(request_type_idx))])
                    .result(Some(ComponentValType::Type(handler_result_type_idx)));
            } else {
                enc.function()
                    .async_(export.is_async)
                    .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                    .result(Some(ComponentValType::Type(result_unit_type)));
            }
        }

        ctx.register_comp_func(&export.name);
        builder.lift_func(
            Some(&export.name),
            ctx.core_func_idx(&core_name),
            func_type,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(ctx.memory_idx()),
            ],
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
fn generate_wasi_imports(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &Project,
) {
    let cli_version = project
        .wasi_registry
        .get_cli_version()
        .expect("WASI CLI version not found in registry - lib/wasi/*.wado not loaded?");

    // Import wasi:cli/types for shared types (error-code)
    let types_instance_type = ctx.register_type("types-instance-type");
    {
        let (_, enc) = builder.ty(Some("types-instance-type"));
        let mut instance_type = InstanceType::new();
        instance_type
            .ty()
            .defined_type()
            .enum_type(["io", "illegal-byte-sequence", "pipe"]);
        instance_type.export(
            "error-code",
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
        "error-code",
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

            let mut resource_type_indices: IndexMap<String, u32> = IndexMap::new();
            let mut own_resource_type_indices: IndexMap<String, u32> = IndexMap::new();
            let mut borrow_resource_type_indices: IndexMap<String, u32> = IndexMap::new();
            for resource_name in &needed_resources {
                if let Some(cm_name) = project.wasi_registry.get_resource_cm_name(resource_name) {
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
            let has_local_error_code = project
                .wasi_registry
                .has_enum_in_interface(&interface_info.path, "ErrorCode");

            // Collect enum types needed by functions
            let mut needed_enums: Vec<String> = Vec::new();
            for func in &supported_functions {
                for (_, _, ty) in &func.params {
                    if let Type::Named(named) = ty
                        && project.wasi_registry.is_enum(&named.name)
                        && !needed_enums.contains(&named.name)
                    {
                        needed_enums.push(named.name.clone());
                    }
                }
                if let Some(ret_ty) = &func.return_type
                    && let Type::Generic(g) = ret_ty
                    && g.name == "Result"
                {
                    for arg in &g.args {
                        if let Type::Named(named) = arg
                            && project.wasi_registry.is_enum(&named.name)
                            && !needed_enums.contains(&named.name)
                        {
                            // Skip ErrorCode for interfaces that don't define their own —
                            // those use the shared wasi:cli/types error-code via outer alias.
                            if named.name == "ErrorCode" && !has_local_error_code {
                                continue;
                            }
                            needed_enums.push(named.name.clone());
                        }
                    }
                }
            }

            // Collect flags types needed by functions
            let mut needed_flags: Vec<String> = Vec::new();
            for func in &supported_functions {
                for (_, _, ty) in &func.params {
                    if let Type::Named(named) = ty
                        && project.wasi_registry.is_flags(&named.name)
                        && !needed_flags.contains(&named.name)
                    {
                        needed_flags.push(named.name.clone());
                    }
                }
            }

            let mut enum_type_indices: IndexMap<String, u32> = IndexMap::new();
            let mut enum_export_indices: IndexMap<String, u32> = IndexMap::new();
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

            // Emit flags types in the instance type
            let mut flags_export_indices: IndexMap<String, u32> = IndexMap::new();
            for flags_name in &needed_flags {
                if let Some(members) = project.wasi_registry.get_flags_members(flags_name) {
                    instance_type
                        .ty()
                        .defined_type()
                        .flags(members.iter().map(String::as_str));
                    let type_idx = local_type_idx;
                    local_type_idx += 1;

                    if let Some(cm_name) = project.wasi_registry.get_flags_cm_name(flags_name) {
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

            for func in &supported_functions {
                let needs_stream_u8 = func
                    .params
                    .iter()
                    .any(|(_, _, ty)| matches!(ty, Type::Generic(g) if g.name == "Stream"));
                let needs_error_code = func
                    .return_type
                    .as_ref()
                    .is_some_and(|ty| matches!(ty, Type::Generic(g) if g.name == "Result"));
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

                let error_code_idx = if needs_error_code {
                    let uses_local_error_code =
                        has_local_error_code && enum_export_indices.contains_key("ErrorCode");

                    if uses_local_error_code {
                        Some(enum_export_indices["ErrorCode"])
                    } else {
                        let outer_error_code = ctx.type_idx("error-code");
                        instance_type.alias(Alias::Outer {
                            kind: ComponentOuterAliasKind::Type,
                            count: 1,
                            index: outer_error_code,
                        });
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    }
                } else {
                    None
                };

                let result_type_idx = if let Some(err_idx) = error_code_idx {
                    let ok_type = if let Some(Type::Generic(g)) = &func.return_type
                        && g.name == "Result"
                        && !g.args.is_empty()
                    {
                        if let Type::Named(named) = &g.args[0]
                            && let Some(&own_idx) = own_resource_type_indices.get(&named.name)
                        {
                            Some(ComponentValType::Type(own_idx))
                        } else if let Type::Named(named) = &g.args[0]
                            && named.name == "()"
                        {
                            None
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    instance_type
                        .ty()
                        .defined_type()
                        .result(ok_type, Some(ComponentValType::Type(err_idx)));
                    let idx = local_type_idx;
                    local_type_idx += 1;
                    Some(idx)
                } else {
                    None
                };

                let result_param_type_idx = if needs_result_param {
                    instance_type.ty().defined_type().result(None, None);
                    let idx = local_type_idx;
                    local_type_idx += 1;
                    Some(idx)
                } else {
                    None
                };

                let array_type_idx = if let Some(Type::Generic(g)) = &func.return_type {
                    if g.name == "Array" && !g.args.is_empty() {
                        let element_type = &g.args[0];
                        let element_val_type = match element_type {
                            Type::Generic(elem_g)
                                if elem_g.name == "Tuple" && !elem_g.args.is_empty() =>
                            {
                                let tuple_types: Vec<ComponentValType> = elem_g
                                    .args
                                    .iter()
                                    .map(|t| {
                                        type_to_cm_primitive_with_resources(
                                            t,
                                            &own_resource_type_indices,
                                        )
                                    })
                                    .collect();
                                instance_type.ty().defined_type().tuple(tuple_types);
                                let tuple_idx = local_type_idx;
                                local_type_idx += 1;
                                ComponentValType::Type(tuple_idx)
                            }
                            Type::Tuple(elems) if !elems.is_empty() => {
                                let tuple_types: Vec<ComponentValType> = elems
                                    .iter()
                                    .map(|t| {
                                        type_to_cm_primitive_with_resources(
                                            t,
                                            &own_resource_type_indices,
                                        )
                                    })
                                    .collect();
                                instance_type.ty().defined_type().tuple(tuple_types);
                                let tuple_idx = local_type_idx;
                                local_type_idx += 1;
                                ComponentValType::Type(tuple_idx)
                            }
                            _ => type_to_cm_primitive_with_resources(
                                element_type,
                                &own_resource_type_indices,
                            ),
                        };
                        instance_type.ty().defined_type().list(element_val_type);
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let option_type_idx = if let Some(Type::Generic(g)) = &func.return_type {
                    if g.name == "Option" && !g.args.is_empty() {
                        let element_type = &g.args[0];
                        let element_val_type = type_to_cm_primitive_with_resources(
                            element_type,
                            &own_resource_type_indices,
                        );
                        instance_type.ty().defined_type().option(element_val_type);
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let tuple_type_idx = if let Some(Type::Tuple(elems)) = &func.return_type {
                    if elems.is_empty() {
                        None
                    } else {
                        let tuple_types = build_cm_tuple_types(
                            elems,
                            &mut instance_type,
                            &mut local_type_idx,
                            error_code_idx,
                            has_local_error_code,
                            &enum_export_indices,
                            &own_resource_type_indices,
                            ctx,
                        );
                        instance_type.ty().defined_type().tuple(tuple_types);
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    }
                } else if let Some(Type::Generic(g)) = &func.return_type {
                    if g.name == "Tuple" && !g.args.is_empty() {
                        let tuple_types = build_cm_tuple_types(
                            &g.args,
                            &mut instance_type,
                            &mut local_type_idx,
                            error_code_idx,
                            has_local_error_code,
                            &enum_export_indices,
                            &own_resource_type_indices,
                            ctx,
                        );
                        instance_type.ty().defined_type().tuple(tuple_types);
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let kebab_params: Vec<(String, ComponentValType)> = func
                    .params
                    .iter()
                    .map(|(_, cm_name, ty)| {
                        let val_type = wado_type_to_cm_val_type(
                            project,
                            ty,
                            stream_type_idx,
                            error_code_idx,
                            result_param_type_idx,
                            &enum_export_indices,
                            &flags_export_indices,
                            &borrow_resource_type_indices,
                        );
                        (cm_name.clone(), val_type)
                    })
                    .collect();
                let params: Vec<(&str, ComponentValType)> = kebab_params
                    .iter()
                    .map(|(name, val_type)| (name.as_str(), *val_type))
                    .collect();

                let result_type = func.return_type.as_ref().map(|ty| {
                    let resolved_ty = project.wasi_registry.resolve_type(ty);
                    wado_type_to_cm_result_type(
                        &resolved_ty,
                        result_type_idx,
                        array_type_idx,
                        option_type_idx,
                        tuple_type_idx,
                    )
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
            if let Some(cm_name) = project.wasi_registry.get_resource_cm_name(resource_name) {
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

    // For Service world, import wasi:http/types
    if project.has_http_handler_export {
        import_http_types_for_service(project, builder, ctx);
    }
}

fn import_http_types_for_service(
    project: &Project,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
) {
    let http_types_instance_type = ctx.register_type("http-types-instance-type");
    {
        let (_, enc) = builder.ty(Some("http-types-instance-type"));
        let mut instance_type = InstanceType::new();

        instance_type.export(
            "request",
            wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
        );
        instance_type.export(
            "response",
            wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
        );
        instance_type.export(
            "fields",
            wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
        );

        // Type generation starts at index 3 (after the 3 SubResource exports).
        // CmInstanceTypeGen emits error-code and its payload structs
        // (DNS-error-payload, TLS-alert-received-payload, field-size-payload)
        // on demand when the parameter/return types of [static]response.new are processed.
        let mut type_gen = CmInstanceTypeGen::new(3);
        let resource_exports: IndexMap<&str, u32> =
            [("request", 0), ("response", 1), ("fields", 2)]
                .into_iter()
                .collect();

        let all_funcs: Vec<WasiFunctionInfo> = project
            .wasi_registry
            .interfaces()
            .find(|i| i.package == "http" && i.interface == "types")
            .map(|i| i.functions)
            .unwrap_or_default();

        // Emit [constructor]fields and [static]response.new from registry metadata.
        // Processing their parameter and return types triggers on-demand emission of
        // all dependent types (error-code variant and its payload record types).
        for func in all_funcs.iter().filter(|f| {
            f.wasi_func_name == "[constructor]fields" || f.wasi_func_name == "[static]response.new"
        }) {
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
            instance_type
                .ty()
                .function()
                .params(param_refs)
                .result(cm_result);
            let func_type_idx = type_gen.alloc_idx();

            instance_type.export(
                &func.wasi_func_name,
                wasm_encoder::ComponentTypeRef::Func(func_type_idx),
            );
        }

        let resource_methods: Vec<WasiFunctionInfo> = all_funcs
            .iter()
            .filter(|f| {
                let is_fields_method =
                    f.effect_name == "Fields" && f.wasi_func_name.starts_with("[method]");
                let is_response_method =
                    f.effect_name == "Response" && f.wasi_func_name.starts_with("[method]");
                // Only include Request methods that are actually used to avoid
                // referencing unsupported resource types (e.g. RequestOptions).
                let is_used_request_method = f.effect_name == "Request"
                    && f.wasi_func_name.starts_with("[method]")
                    && project
                        .used_wasi_functions
                        .contains(&format!("{}::{}", f.effect_name, f.method_name));
                is_fields_method || is_response_method || is_used_request_method
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
            instance_type
                .ty()
                .function()
                .params(param_refs)
                .result(cm_result);
            let func_type_idx = type_gen.alloc_idx();

            instance_type.export(
                &func.wasi_func_name,
                wasm_encoder::ComponentTypeRef::Func(func_type_idx),
            );
        }

        enc.instance(&instance_type);
    }

    ctx.register_instance("http-types");
    let http_version = "0.3.0-rc-2026-01-06";
    let http_types_import_path = format!("wasi:http/types@{http_version}");
    builder.import(
        &http_types_import_path,
        wasm_encoder::ComponentTypeRef::Instance(http_types_instance_type),
    );

    ctx.register_type("http-request-resource");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        "request",
        ComponentExportKind::Type,
    );
    ctx.register_type("http-response-resource");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        "response",
        ComponentExportKind::Type,
    );
    ctx.register_type("http-fields-resource");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        "fields",
        ComponentExportKind::Type,
    );
    ctx.register_type("http-error-code");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        "error-code",
        ComponentExportKind::Type,
    );

    ctx.register_comp_func("http-fields-constructor");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        "[constructor]fields",
        ComponentExportKind::Func,
    );
    ctx.register_comp_func("http-response-new");
    builder.alias_export(
        ctx.instance_idx("http-types"),
        "[static]response.new",
        ComponentExportKind::Func,
    );
    ctx.alias_comp_func("http-fields-constructor", "wasi:http/Fields::new");

    // Alias Fields, Response, and used Request resource methods
    {
        let resource_funcs: Vec<(String, String)> = project
            .wasi_registry
            .interfaces()
            .find(|i| i.package == "http" && i.interface == "types")
            .map(|i| {
                i.functions
                    .iter()
                    .filter(|f| {
                        let is_fields_method =
                            f.effect_name == "Fields" && f.wasi_func_name.starts_with("[method]");
                        let is_response_method =
                            f.effect_name == "Response" && f.wasi_func_name.starts_with("[method]");
                        let is_used_request_method = f.effect_name == "Request"
                            && f.wasi_func_name.starts_with("[method]")
                            && project
                                .used_wasi_functions
                                .contains(&format!("{}::{}", f.effect_name, f.method_name));
                        is_fields_method || is_response_method || is_used_request_method
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

    // Define own<request> type
    let request_resource_idx = ctx.type_idx("http-request-resource");
    ctx.register_type("http-request");
    {
        let (_, enc) = builder.ty(Some("http-request"));
        enc.defined_type().own(request_resource_idx);
    }

    // Define own<response> type
    let response_resource_idx = ctx.type_idx("http-response-resource");
    ctx.register_type("http-response");
    {
        let (_, enc) = builder.ty(Some("http-response"));
        enc.defined_type().own(response_resource_idx);
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

fn import_interface_with_resource(
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    interface_info: &crate::component_model::WasiInterfaceInfo,
    project: &Project,
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
    project: &Project,
) {
    let interfaces_with_resources: Vec<_> = project
        .wasi_registry
        .interfaces()
        .filter(|info| info.resource_type.is_some() && info.package != "http")
        .collect();

    // Phase 1: Import resource-defining interfaces
    let mut imported_source_interfaces: IndexSet<String> = IndexSet::new();
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

        let Some(wasi_import) = crate::ast::WasiImport::parse(source_path) else {
            continue;
        };

        let instance_type_name = format!("{}-instance-type", wasi_import.interface);
        let instance_type_idx = ctx.register_type(&instance_type_name);
        {
            let (_, enc) = builder.ty(Some(&instance_type_name));
            let mut instance_type = InstanceType::new();
            instance_type.export(
                resource_cm_name,
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );
            enc.instance(&instance_type);
        }

        ctx.register_instance(&wasi_import.interface);
        builder.import(
            source_path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        let resource_type_name = format!("resource:{resource_cm_name}");
        ctx.register_type(&resource_type_name);
        builder.alias_export(
            ctx.instance_idx(&wasi_import.interface),
            resource_cm_name,
            ComponentExportKind::Type,
        );
    }

    // Phase 2: Import function interfaces that use those resources
    for interface_info in &interfaces_with_resources {
        import_interface_with_resource(builder, ctx, interface_info, project);
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
    project: &Project,
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
        // Interfaces with no resources are already handled in generate_wasi_imports.
        if needed_resources.is_empty() {
            continue;
        }

        // Skip if already imported (e.g., previously handled by generate_wasi_imports on a prior build)
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
            if let Some(cm_name) = project.wasi_registry.get_resource_cm_name(resource_name) {
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
                let Some(wasi_import) = crate::ast::WasiImport::parse(source_path) else {
                    continue;
                };
                // Use package-qualified names to avoid collision with same-named interfaces
                // from different packages (e.g., wasi:cli/types vs wasi:filesystem/types).
                let src_instance_name =
                    format!("{}-{}", wasi_import.package, wasi_import.interface);
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
            let mut resource_alias_indices: IndexMap<String, u32> = IndexMap::new();
            let mut own_resource_type_indices: IndexMap<String, u32> = IndexMap::new();
            let mut borrow_resource_type_indices: IndexMap<String, u32> = IndexMap::new();

            for resource_name in &needed_resources {
                if let Some(cm_name) = project.wasi_registry.get_resource_cm_name(resource_name) {
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
                        continue;
                    }
                }
            }

            // Build function types using the aliased resource indices
            let mut deferred_func_exports: Vec<(String, u32)> = Vec::new();

            for func in &supported_functions {
                // Build return type
                let array_type_idx = if let Some(Type::Generic(g)) = &func.return_type {
                    if g.name == "Array" && !g.args.is_empty() {
                        let element_type = &g.args[0];
                        let element_val_type = match element_type {
                            Type::Generic(elem_g)
                                if elem_g.name == "Tuple" && !elem_g.args.is_empty() =>
                            {
                                let tuple_types: Vec<ComponentValType> = elem_g
                                    .args
                                    .iter()
                                    .map(|t| {
                                        type_to_cm_primitive_with_resources(
                                            t,
                                            &own_resource_type_indices,
                                        )
                                    })
                                    .collect();
                                instance_type.ty().defined_type().tuple(tuple_types);
                                let tuple_idx = local_type_idx;
                                local_type_idx += 1;
                                ComponentValType::Type(tuple_idx)
                            }
                            Type::Tuple(elems) if !elems.is_empty() => {
                                let tuple_types: Vec<ComponentValType> = elems
                                    .iter()
                                    .map(|t| {
                                        type_to_cm_primitive_with_resources(
                                            t,
                                            &own_resource_type_indices,
                                        )
                                    })
                                    .collect();
                                instance_type.ty().defined_type().tuple(tuple_types);
                                let tuple_idx = local_type_idx;
                                local_type_idx += 1;
                                ComponentValType::Type(tuple_idx)
                            }
                            _ => type_to_cm_primitive_with_resources(
                                element_type,
                                &own_resource_type_indices,
                            ),
                        };
                        instance_type.ty().defined_type().list(element_val_type);
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let result_type = func.return_type.as_ref().map(|ty| {
                    let resolved_ty = project.wasi_registry.resolve_type(ty);
                    wado_type_to_cm_result_type(&resolved_ty, None, array_type_idx, None, None)
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
    project: &Project,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
) {
    for interface_info in project.wasi_registry.interfaces() {
        for func in &interface_info.functions {
            let local_name = func.local_alias_name();

            if !ctx.has_comp_func(&local_name) {
                continue;
            }

            if func.is_async && func.return_type.is_none() {
                continue;
            }

            ctx.register_core_func(&local_name);

            let mut options: Vec<CanonicalOption> = Vec::new();

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

fn append_http_handler_export(component_bytes: &mut Vec<u8>, ctx: &ComponentModelContext) {
    use wasm_encoder::{ComponentExportSection, ComponentInstanceSection, ComponentSection};

    let handle_func_idx = ctx.comp_func_idx("handle");

    let request_type_idx = ctx.type_idx("http-request-resource");
    let response_type_idx = ctx.type_idx("http-response-resource");
    let error_code_type_idx = ctx.type_idx("http-error-code");

    let mut instances = ComponentInstanceSection::new();
    instances.export_items([
        ("request", ComponentExportKind::Type, request_type_idx),
        ("response", ComponentExportKind::Type, response_type_idx),
        ("error-code", ComponentExportKind::Type, error_code_type_idx),
        ("handle", ComponentExportKind::Func, handle_func_idx),
    ]);

    let instance_idx = ctx.instance_count();

    let mut exports = ComponentExportSection::new();
    let http_version = "0.3.0-rc-2026-01-06";
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
