//! The core exports a host calls a Wado closure back through, one per
//! argument shape. The component lifts none of them: the closure arrives as
//! the `externref` it crossed as, which the Component Model cannot carry.

use crate::hashmap::IndexSet;
use crate::name::{CALLBACK_HANDLE_WORD, callback_export_name, cm_callback_func_name};
use crate::package::Package;
use crate::synthesis::common::{
    block, builtin_call, expr_stmt, local_ref, locals_from_params, synth_param, synth_span,
};
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TypeId};
use crate::type_table::TypeTable;

use super::entry_type_table;
use super::import_adapter::make_binding_function;

/// The closure types the imports are passed, each with the extern handle type
/// it crosses as.
pub(super) type Callbacks = IndexSet<(TypeId, TypeId)>;

/// Add an export for each argument shape among `callbacks` to the entry
/// module.
pub(super) fn synthesize_callback_exports(project: &mut Package, callbacks: &Callbacks) {
    let type_table = entry_type_table(project);
    let mut shapes: IndexSet<String> = IndexSet::default();
    let mut functions = Vec::new();
    for &(closure, extern_object) in callbacks {
        let params = closure_params(&type_table.borrow(), closure);
        let shape = callback_export_name(
            params
                .iter()
                .map(|&param| argument_word(&type_table.borrow(), param)),
        );
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
            TypeTable::UNIT,
            synth_span(),
        );
        let locals = locals_from_params(&tir_params);
        let function = make_binding_function(
            cm_callback_func_name(&shape),
            tir_params,
            TypeTable::UNIT,
            block(vec![expr_stmt(call)]),
            u32::try_from(locals.len()).expect("a closure takes few parameters"),
            locals,
        );
        {
            let mut function = function.borrow_mut();
            function.is_export = true;
            // The host calls it, so it is a root with a fixed signature.
            function.is_cm_export = true;
            project.callback_core_exports.push(function.name.clone());
        }
        functions.push(function);
    }
    project
        .tir_modules
        .get_mut(&project.entry_module_source)
        .expect("entry module should exist")
        .functions
        .extend(functions);
}

fn closure_params(type_table: &TypeTable, closure: TypeId) -> Vec<TypeId> {
    let ResolvedType::Function {
        params,
        return_type,
        ..
    } = type_table.get(closure)
    else {
        unreachable!("an import takes a callback as a closure");
    };
    assert_eq!(
        *return_type,
        TypeTable::UNIT,
        "the elaborator admits only a callback returning nothing"
    );
    params.clone()
}

/// What the export's name calls a closure argument of type `param`. One name
/// must mean one closure type in the guest, where every extern handle is one
/// `externref`.
fn argument_word(type_table: &TypeTable, param: TypeId) -> &'static str {
    if type_table.is_extern_handle(param) {
        return CALLBACK_HANDLE_WORD;
    }
    let head = type_table.representation_head(param);
    let ResolvedType::Primitive(primitive) = type_table.get(head) else {
        unreachable!("the elaborator admits only a scalar callback parameter");
    };
    primitive.as_str()
}
