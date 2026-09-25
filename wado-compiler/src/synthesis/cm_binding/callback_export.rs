//! The exports a host calls a Wado closure back through, one per signature.

use std::cell::RefCell;
use std::iter;

use crate::hashmap::{IndexMap, IndexSet};
use crate::name::callback_export_name;
use crate::package::Package;
use crate::synthesis::common::{
    block, cast, expr_stmt, handle_from_f64, internal_call, local_ref, locals_from_params,
    synth_span,
};
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirParam, TypeId, TypeTable};
use crate::world_registry::CallbackExport;

use super::entry_type_table;
use super::export_adapter::export_binding_func_name;
use super::import_adapter::make_binding_function;

/// Add an export for each boundary signature among `callbacks`, the closure
/// types the imports take, to the entry module.
pub(super) fn synthesize_callback_exports(project: &mut Package, callbacks: &IndexSet<TypeId>) {
    let type_table = entry_type_table(project);
    let mut exports: IndexMap<String, CallbackExport> = IndexMap::default();
    let mut functions = Vec::new();
    for &closure in callbacks {
        let params = closure_params(&type_table.borrow(), closure);
        let boundary: Vec<Boundary> = params
            .iter()
            .map(|&param| boundary(&type_table.borrow(), param))
            .collect();
        let cm_name = callback_export_name(boundary.iter().map(|b| b.word));
        if exports.contains_key(&cm_name) {
            continue;
        }
        let core_func = export_binding_func_name(&cm_name);
        let mut tir_params = vec![param("key", 0, TypeTable::U32)];
        let mut args = Vec::new();
        for (i, (&wado, b)) in params.iter().zip(&boundary).enumerate() {
            let name = format!("a{i}");
            let slot = u32::try_from(i + 1).expect("a closure takes few parameters");
            tir_params.push(param(&name, slot, b.type_id));
            args.push(lift_arg(
                &type_table,
                local_ref(slot, &name, b.type_id),
                wado,
            ));
        }
        let erased = erased_callback_type(&mut type_table.borrow_mut());
        let callee = cast(
            internal_call(
                "cm_callback",
                vec![local_ref(0, "key", TypeTable::U32)],
                erased,
            ),
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
            core_func.clone(),
            tir_params,
            TypeTable::UNIT,
            block(vec![expr_stmt(call)]),
            u32::try_from(locals.len()).expect("a closure takes few parameters"),
            locals,
        );
        {
            let mut function = function.borrow_mut();
            function.is_export = true;
            function.is_cm_export = true;
        }
        functions.push(function);
        let export_params = iter::once(("key".to_string(), "u32"))
            .chain(
                boundary
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (format!("a{i}"), b.primitive)),
            )
            .collect();
        exports.insert(
            cm_name.clone(),
            CallbackExport {
                cm_name,
                core_func,
                params: export_params,
            },
        );
    }
    project
        .tir_modules
        .get_mut(&project.entry_module_source)
        .expect("entry module should exist")
        .functions
        .extend(functions);
    project.callback_exports = exports.into_values().collect();
}

/// The `fn()` the `core:rt` registry holds every callback as.
pub(super) fn erased_callback_type(type_table: &mut TypeTable) -> TypeId {
    type_table.make_function(vec![], TypeTable::UNIT, vec![])
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

/// How a closure argument crosses the boundary.
struct Boundary {
    type_id: TypeId,
    /// The Wado primitive `type_id` is.
    primitive: &'static str,
    /// What the export's name calls it. One name must mean one closure type in
    /// the guest, where a handle crossing as an `f64` is an `i64`.
    word: &'static str,
}

fn boundary(type_table: &TypeTable, param: TypeId) -> Boundary {
    if type_table.is_unrestricted_handle(param) {
        return Boundary {
            type_id: TypeTable::F64,
            primitive: "f64",
            word: "handle",
        };
    }
    let head = type_table.representation_head(param);
    let ResolvedType::Primitive(primitive) = type_table.get(head) else {
        unreachable!("the elaborator admits only a scalar callback parameter");
    };
    Boundary {
        type_id: head,
        primitive: primitive.as_str(),
        word: primitive.as_str(),
    }
}

/// `value`, at its boundary type, as the Wado `param` the closure takes.
fn lift_arg(type_table: &RefCell<TypeTable>, value: TirExpr, param: TypeId) -> TirExpr {
    if type_table.borrow().is_unrestricted_handle(param) {
        return handle_from_f64(value, param);
    }
    if value.type_id == param {
        return value;
    }
    cast(value, param)
}

fn param(name: &str, local_index: u32, type_id: TypeId) -> TirParam {
    TirParam {
        name: name.to_string(),
        type_id,
        local_index,
        is_mut: false,
        is_mut_ref: false,
        span: synth_span(),
    }
}
