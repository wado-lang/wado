//! The core exports a host of the web imports calls: one per argument shape
//! to call a Wado closure back through, and the pair that reaches the slots of
//! extern handles. The component lifts none of them: each takes or answers an
//! `externref`, which the Component Model cannot carry.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::hashmap::IndexSet;
use crate::name::{
    CALLBACK_HANDLE_WORD, ExternSlotAccess, callback_export_name, cm_callback_func_name,
};
use crate::package::Package;
use crate::synthesis::common::{
    block, builtin_call, expr_stmt, internal_call, local_ref, locals_from_params, return_stmt,
    synth_param, synth_span,
};
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirFunction, TypeId};
use crate::type_table::TypeTable;

use super::entry_type_table;
use super::import_adapter::make_binding_function;

/// The closure types the imports are passed, each with the extern handle type
/// it crosses as.
pub(super) type Callbacks = IndexSet<(TypeId, TypeId)>;

/// What the calls of web imports ask the main module to export.
#[derive(Default)]
pub(super) struct HostCalls {
    pub callbacks: Callbacks,
    /// Whether a call puts an extern handle in a slot, which the host then
    /// reaches.
    pub extern_slots: bool,
}

/// Add an export for each argument shape among `callbacks` to the entry
/// module.
pub(super) fn synthesize_callback_exports(project: &mut Package, callbacks: &Callbacks) {
    let type_table = entry_type_table(project);
    let mut shapes: IndexSet<String> = IndexSet::default();
    for &(closure, extern_object) in callbacks {
        let (params, result) = type_table
            .borrow()
            .function_signature(closure)
            .map(|(params, result)| (params.to_vec(), result))
            .expect("an import takes a callback as a closure");
        let shape = {
            let type_table = type_table.borrow();
            let word = |ty: TypeId| value_word(&type_table, ty);
            callback_export_name(
                params.iter().map(|&param| word(param)),
                (result != TypeTable::UNIT).then(|| word(result)),
            )
        };
        if !shapes.insert(shape.clone()) {
            continue;
        }
        let mut tir_params = vec![synth_param("closure", 0, extern_object)];
        let mut args = Vec::new();
        for (i, &param) in params.iter().enumerate() {
            let name = format!("a{i}");
            let slot = u32::try_from(i + 1).expect("a closure takes few parameters");
            tir_params.push(synth_param(&name, slot, param));
            args.push(local_ref(slot, &name, param));
        }
        let callee = builtin_call(
            "extern_as_closure",
            vec![local_ref(0, "closure", extern_object)],
            closure,
        );
        let call = TirExpr::new(
            TirExprKind::IndirectCall {
                callee: Box::new(callee),
                args,
            },
            result,
            synth_span(),
        );
        let body = if result == TypeTable::UNIT {
            expr_stmt(call)
        } else {
            return_stmt(Some(call))
        };
        let locals = locals_from_params(&tir_params);
        let function = make_binding_function(
            cm_callback_func_name(&shape),
            tir_params,
            result,
            block(vec![body]),
            u32::try_from(locals.len()).expect("a closure takes few parameters"),
            locals,
        );
        export_to_host(project, function);
    }
}

/// Add the exports the host reaches an extern handle's slot through
/// ([`ExternSlotAccess`]) to the entry module.
pub(super) fn synthesize_extern_slot_exports(project: &mut Package) {
    let extern_object = entry_type_table(project).borrow_mut().make_extern_object();
    let accessors = [
        (
            ExternSlotAccess::Put,
            CompilerItem::ExternSlotPut,
            extern_object,
            TypeTable::I32,
        ),
        (
            ExternSlotAccess::Get,
            CompilerItem::ExternSlotGet,
            TypeTable::I32,
            extern_object,
        ),
    ];
    for (access, item, param, result) in accessors {
        let params = vec![synth_param("a0", 0, param)];
        let call = internal_call(item.attr_name(), vec![local_ref(0, "a0", param)], result);
        let locals = locals_from_params(&params);
        let function = make_binding_function(
            access.export_name(),
            params,
            result,
            block(vec![return_stmt(Some(call))]),
            1,
            locals,
        );
        export_to_host(project, function);
    }
}

/// Make `function` a core export of the main module under its own name, in
/// the entry module. The host calls it, so it is a root with a fixed
/// signature.
fn export_to_host(project: &mut Package, function: Rc<RefCell<TirFunction>>) {
    {
        let mut function = function.borrow_mut();
        function.is_export = true;
        function.is_cm_export = true;
        project.host_core_exports.push(function.name.clone());
    }
    project
        .tir_modules
        .get_mut(&project.entry_module_source)
        .expect("entry module should exist")
        .functions
        .push(function);
}

/// What the export's name calls a closure argument or result of type `ty`.
/// One name must mean one closure type in the guest, where every extern handle
/// is one `externref`.
fn value_word(type_table: &TypeTable, ty: TypeId) -> &'static str {
    if type_table.is_extern_handle(ty) {
        return CALLBACK_HANDLE_WORD;
    }
    let head = type_table.representation_head(ty);
    let ResolvedType::Primitive(primitive) = type_table.get(head) else {
        unreachable!("the elaborator admits only a scalar or a handle in a callback");
    };
    primitive.as_str()
}
