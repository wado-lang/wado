//! A dependency whose guest effect a handler of the program answers. The
//! dependency imports the effect's interface, and the program calls into the
//! dependency, so neither can be instantiated first. The program's component
//! nests the dependency and instantiates it against a lift of each operation's
//! binding, the shape `Linking.md` calls donut wrapping: the lift calls through
//! a funcref table that is filled once the main module exists, and uses the
//! program's memory, which the `mem` instance defines before either.

use std::borrow::Cow;

use wasm_encoder::{
    CanonicalOption, CodeSection, ComponentExportKind, ComponentValType, ConstExpr, ElementSection,
    Elements, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Module, ModuleArg, RefType, TableSection, TableType, TypeSection, ValType,
};

use super::{alias_interface_func, collect_component_asset, emits_function};
use crate::ast::{AstId, CmImport, NamedType, Type};
use crate::codegen::component_context::ComponentModelContext;
use crate::component_model::{
    CmDefined, CmInterfaceInfo, CmTypeGen, CmTypeSink, cm_instance_key, emit_cm_defined,
};
use crate::hashmap::IndexMap;
use crate::nir_package::{NestedDependency, NirPackage};
use crate::token::Span;

/// The table every guest callback's lift calls through, and what fills it.
pub(super) struct CallbackTable {
    table: u32,
    /// The main module's export each slot holds, in slot order, with its core
    /// type.
    slots: Vec<(String, CoreFuncType)>,
}

type CoreFuncType = (Vec<ValType>, Vec<ValType>);

/// Lift each guest callback through the table, nest every dependency in
/// `nested` against those lifts, and stand each one's exported instance where
/// the program would have imported it. `None` when nothing is nested.
pub(super) fn nest_dependencies(
    builder: &mut wasm_encoder::ComponentBuilder,
    ctx: &mut ComponentModelContext,
    project: &NirPackage,
    core_module: &[u8],
    nested: &[NestedDependency<'_>],
) -> Option<CallbackTable> {
    if nested.is_empty() {
        return None;
    }
    let callbacks = &project.component_plan.guest_callbacks;
    let slots: Vec<(String, CoreFuncType)> = callbacks
        .iter()
        .flat_map(|callback| {
            std::iter::once(&callback.core_func_name).chain(&callback.post_return_core_name)
        })
        .map(|name| (name.clone(), core_export_type(core_module, name)))
        .collect();

    ctx.register_core_module("guest-callback-shim");
    builder.core_module_raw(Some("guest-callback-shim"), &shim_module(&slots));
    ctx.register_core_instance("guest-callback-shim");
    builder.core_instantiate(
        Some("guest-callback-shim"),
        ctx.core_module_idx("guest-callback-shim"),
        Vec::<(&str, ModuleArg)>::new(),
    );
    let shim = ctx.core_instance_idx("guest-callback-shim");
    let table = builder.core_alias_export(
        Some("guest-callback-table"),
        shim,
        "table",
        ExportKind::Table,
    );
    for (slot, (name, _)) in slots.iter().enumerate() {
        let alias = slot_alias(name);
        ctx.register_core_func(&alias);
        builder.core_alias_export(Some(&alias), shim, &slot.to_string(), ExportKind::Func);
    }

    // One instance per interface the program defines, holding its operations'
    // lifts and the named types it declares.
    let registry = &project.cm_interface_registry;
    let no_resources: IndexMap<&str, u32> = IndexMap::default();
    let mut instances: IndexMap<&str, Vec<(String, ComponentExportKind, u32)>> =
        IndexMap::default();
    let mut type_gens: IndexMap<&str, CmTypeGen> = IndexMap::default();
    for callback in callbacks {
        let func = registry
            .get_function(&callback.function_key)
            .expect("a guest callback names a registered operation");
        let interface = callback.interface_fq.as_str();
        let type_gen = type_gens
            .entry(interface)
            .or_insert_with(|| CmTypeGen::with_interface_hint(interface));
        let exports = instances.entry(interface).or_default();
        let mut val_type = |builder: &mut wasm_encoder::ComponentBuilder,
                            ctx: &mut ComponentModelContext,
                            exports: &mut Vec<(String, ComponentExportKind, u32)>,
                            ty| {
            let resolved = registry.resolve_type_preserving_local_newtypes(ty);
            let mut sink = InstanceExportSink {
                builder,
                ctx,
                exports,
            };
            type_gen.ast_type_to_cm(&mut sink, &resolved, registry, &no_resources)
        };
        let params: Vec<(&str, ComponentValType)> = func
            .params
            .iter()
            .map(|(_, cm_name, ty)| (cm_name.as_str(), val_type(builder, ctx, exports, ty)))
            .collect();
        let result = func
            .return_type
            .as_ref()
            .map(|ty| val_type(builder, ctx, exports, ty));
        let func_type_name = format!("guest-callback-type:{}", callback.function_key);
        let func_type = ctx.register_type(&func_type_name);
        let (_, enc) = builder.ty(Some(&func_type_name));
        enc.function().params(params).result(result);

        let mut options = vec![
            CanonicalOption::Memory(ctx.memory_idx()),
            CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
        ];
        if let Some(post_return) = &callback.post_return_core_name {
            options.push(CanonicalOption::PostReturn(
                ctx.core_func_idx(&slot_alias(post_return)),
            ));
        }
        let lift_name = format!("guest-callback:{}", callback.function_key);
        let lifted = ctx.register_comp_func(&lift_name);
        builder.lift_func(
            Some(&lift_name),
            ctx.core_func_idx(&slot_alias(&callback.core_func_name)),
            func_type,
            options,
        );
        exports.push((
            func.wasi_func_name.clone(),
            ComponentExportKind::Func,
            lifted,
        ));
    }
    // A types-only interface carries nothing to run, so the program defines its
    // types rather than importing it on the dependency's behalf.
    for interface in nested.iter().flat_map(|dependency| dependency.imports) {
        let interface = interface.as_str();
        if registry.declares_functions(interface) {
            continue;
        }
        let type_gen = type_gens
            .entry(interface)
            .or_insert_with(|| CmTypeGen::with_interface_hint(interface));
        let exports = instances.entry(interface).or_default();
        for name in registry.declared_type_names(interface) {
            let mut sink = InstanceExportSink {
                builder,
                ctx,
                exports,
            };
            if let Some((cm_name, base)) = registry.newtype(interface, name) {
                type_gen.define_alias(
                    &mut sink,
                    interface,
                    name,
                    cm_name,
                    base,
                    registry,
                    &no_resources,
                );
            } else {
                let named = NamedType::new(AstId::fresh(), name.to_string(), Span::default());
                registry.set_source_interface(named.id, interface.to_string());
                type_gen.ast_type_to_cm(&mut sink, &Type::Named(named), registry, &no_resources);
            }
        }
    }
    for (interface, exports) in &instances {
        let key = defined_instance_key(interface);
        ctx.register_instance(&key);
        builder.instantiate_exports(
            Some(&key),
            exports
                .iter()
                .map(|(name, kind, idx)| (name.as_str(), *kind, *idx)),
        );
    }

    for (index, dependency) in nested.iter().enumerate() {
        let component = builder.component_raw(
            Some(&format!("dependency-{index}")),
            &collect_component_asset(dependency.asset, project),
        );
        let args: Vec<(&str, ComponentExportKind, u32)> = dependency
            .imports
            .iter()
            .map(|fq| {
                let instance = if instances.contains_key(fq.as_str()) {
                    ctx.instance_idx(&defined_instance_key(fq))
                } else {
                    let import = CmImport::parse(fq).expect("an imported interface FQ");
                    ctx.instance_idx(&cm_instance_key(&import.package, &import.interface))
                };
                (fq.as_str(), ComponentExportKind::Instance, instance)
            })
            .collect();
        let instance_key = format!("dependency-{index}");
        ctx.register_instance(&instance_key);
        builder.instantiate(Some(&instance_key), component, args);
        let dependency_instance = ctx.instance_idx(&instance_key);

        for (export, import) in &dependency.provides {
            let interface = registry_interface(project, import);
            ctx.register_instance(&interface.instance_key());
            builder.alias_export(dependency_instance, export, ComponentExportKind::Instance);
            for func in interface
                .functions
                .iter()
                .filter(|func| emits_function(project, func))
            {
                alias_interface_func(builder, ctx, &interface, func);
            }
        }
    }

    Some(CallbackTable { table, slots })
}

/// Fill the table with the main module's exports, now that it is instantiated.
pub(super) fn fill_callback_table(
    builder: &mut wasm_encoder::ComponentBuilder,
    ctx: &mut ComponentModelContext,
    callback_table: &CallbackTable,
) {
    let main = ctx.core_instance_idx("main");
    let mut args: Vec<(String, ExportKind, u32)> =
        vec![("table".to_string(), ExportKind::Table, callback_table.table)];
    for (slot, (name, _)) in callback_table.slots.iter().enumerate() {
        let alias = format!("{name}-core");
        let idx = ctx.register_core_func(&alias);
        builder.core_alias_export(Some(&alias), main, name, ExportKind::Func);
        args.push((slot.to_string(), ExportKind::Func, idx));
    }
    ctx.register_core_instance("guest-callback-fill-args");
    let fill_args = builder.core_instantiate_exports(
        Some("guest-callback-fill-args"),
        args.iter()
            .map(|(name, kind, idx)| (name.as_str(), *kind, *idx)),
    );
    ctx.register_core_module("guest-callback-fill");
    builder.core_module_raw(
        Some("guest-callback-fill"),
        &fill_module(&callback_table.slots),
    );
    ctx.register_core_instance("guest-callback-fill");
    builder.core_instantiate(
        Some("guest-callback-fill"),
        ctx.core_module_idx("guest-callback-fill"),
        [("", ModuleArg::Instance(fill_args))],
    );
}

fn slot_alias(core_name: &str) -> String {
    format!("guest-callback-slot:{core_name}")
}

/// The instance the program defines for `interface`, rather than importing.
fn defined_instance_key(interface: &str) -> String {
    format!("defined:{interface}")
}

/// The registry's interface `fq`, which the program imports.
fn registry_interface(project: &NirPackage, fq: &str) -> CmInterfaceInfo {
    project
        .cm_interface_registry
        .interfaces()
        .find(|interface| interface.path == fq)
        .unwrap_or_else(|| panic!("`{fq}` is not a registered interface"))
}

/// The table each slot's trampoline calls through: slot `i` is export `"i"`,
/// which calls whatever the table holds at `i` with its own arguments.
fn shim_module(slots: &[(String, CoreFuncType)]) -> Vec<u8> {
    let mut types = TypeSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut tables = TableSection::new();
    tables.table(slot_table_type(slots));
    exports.export("table", ExportKind::Table, 0);
    for (slot, (_, (params, results))) in slots.iter().enumerate() {
        let index = u32::try_from(slot).expect("a program declares few callbacks");
        types
            .ty()
            .function(params.iter().copied(), results.iter().copied());
        functions.function(index);
        exports.export(&slot.to_string(), ExportKind::Func, index);
        let mut body = Function::new([]);
        let mut sink = body.instructions();
        for param in 0..params.len() {
            sink.local_get(u32::try_from(param).expect("a core signature has few params"));
        }
        sink.i32_const(i32::try_from(index).expect("a program declares few callbacks"))
            .call_indirect(0, index)
            .end();
        code.function(&body);
    }
    let mut module = Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&tables)
        .section(&exports)
        .section(&code);
    module.finish()
}

/// Write each slot's function into the shim's table.
fn fill_module(slots: &[(String, CoreFuncType)]) -> Vec<u8> {
    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    imports.import("", "table", slot_table_type(slots));
    for (slot, (_, (params, results))) in slots.iter().enumerate() {
        types
            .ty()
            .function(params.iter().copied(), results.iter().copied());
        let index = u32::try_from(slot).expect("a program declares few callbacks");
        imports.import("", &slot.to_string(), EntityType::Function(index));
    }
    let functions: Vec<u32> = (0..slots.len())
        .map(|slot| u32::try_from(slot).expect("a program declares few callbacks"))
        .collect();
    let mut elements = ElementSection::new();
    elements.active(
        Some(0),
        &ConstExpr::i32_const(0),
        Elements::Functions(Cow::Owned(functions)),
    );
    let mut module = Module::new();
    module.section(&types).section(&imports).section(&elements);
    module.finish()
}

fn slot_table_type(slots: &[(String, CoreFuncType)]) -> TableType {
    let size = u64::try_from(slots.len()).expect("a program declares few callbacks");
    TableType {
        element_type: RefType::FUNCREF,
        table64: false,
        minimum: size,
        maximum: Some(size),
        shared: false,
    }
}

/// The core signature of `module`'s function export `name`.
fn core_export_type(module: &[u8], name: &str) -> CoreFuncType {
    let mut func_types: Vec<Option<wasmparser::FuncType>> = Vec::new();
    let mut funcs: Vec<u32> = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        match payload.expect("the main module is valid") {
            wasmparser::Payload::TypeSection(reader) => {
                for rec_group in reader {
                    for sub_type in rec_group.expect("valid type").types() {
                        func_types.push(match &sub_type.composite_type.inner {
                            wasmparser::CompositeInnerType::Func(func) => Some(func.clone()),
                            wasmparser::CompositeInnerType::Array(_)
                            | wasmparser::CompositeInnerType::Struct(_)
                            | wasmparser::CompositeInnerType::Cont(_) => None,
                        });
                    }
                }
            }
            wasmparser::Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    if let wasmparser::TypeRef::Func(ty) = import.expect("valid import").ty {
                        funcs.push(ty);
                    }
                }
            }
            wasmparser::Payload::FunctionSection(reader) => {
                funcs.extend(reader.into_iter().map(|ty| ty.expect("valid function")));
            }
            wasmparser::Payload::ExportSection(reader) => {
                for export in reader {
                    let export = export.expect("valid export");
                    if export.name == name {
                        let ty = funcs[export.index as usize];
                        let func = func_types[ty as usize]
                            .as_ref()
                            .expect("an export's type is a function type");
                        return (
                            func.params().iter().copied().map(core_val_type).collect(),
                            func.results().iter().copied().map(core_val_type).collect(),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    panic!("the main module exports no `{name}`")
}

fn core_val_type(ty: wasmparser::ValType) -> ValType {
    match ty {
        wasmparser::ValType::I32 => ValType::I32,
        wasmparser::ValType::I64 => ValType::I64,
        wasmparser::ValType::F32 => ValType::F32,
        wasmparser::ValType::F64 => ValType::F64,
        wasmparser::ValType::V128 | wasmparser::ValType::Ref(_) => {
            unreachable!("a canonical ABI signature carries only numbers")
        }
    }
}

/// Defines the types a callback's signature reaches at the top level, and
/// records each named one for the interface's instance to export.
struct InstanceExportSink<'a> {
    builder: &'a mut wasm_encoder::ComponentBuilder,
    ctx: &'a mut ComponentModelContext,
    exports: &'a mut Vec<(String, ComponentExportKind, u32)>,
}

impl CmTypeSink for InstanceExportSink<'_> {
    fn define(&mut self, defined: CmDefined<'_>) -> u32 {
        let idx = self.ctx.register_anon_type();
        let (_, enc) = self.builder.ty(None);
        emit_cm_defined(enc.defined_type(), defined);
        idx
    }

    fn name(&mut self, cm_name: &str, idx: u32) -> u32 {
        self.exports
            .push((cm_name.to_string(), ComponentExportKind::Type, idx));
        idx
    }
}
