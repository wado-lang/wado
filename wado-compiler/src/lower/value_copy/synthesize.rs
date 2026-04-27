//! Replace each `builtin::copy_value::<T>(x)` call with a call to a
//! synthesized concrete `$value_copy$T_<id>` function carrying
//! `FunctionKind::ValueCopy { type_id }`. Runs at the end of `lower`,
//! immediately after `value_copy::insert::insert_value_copy_calls`.
//!
//! For struct types the body is a `StructLiteral` with field-by-field
//! shallow projections, plus `builtin::array_clone::<T>` for raw
//! `builtin::array<T>` fields — a one-level shallow copy that does not
//! recurse into nested aggregates.
//!
//! For variant / option / fall-through types the body is `return v;`
//! (identity).

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType, TirBlock, TirExpr,
    TirExprKind, TirField, TirFunction, TirParam, TirStmt, TirStmtKind, TirStruct, TirStructField,
    TypeId, TypeTable,
};
use crate::tir_visitor::{TirOptVisitor, TirRefVisitor, opt_walk_block, opt_walk_expr};
use crate::token::Span;

pub fn synthesize_value_copy_funcs(project: &mut FlatPackage) {
    let copy_types = collect_copy_value_types(project);
    if copy_types.is_empty() {
        return;
    }

    let type_table_rc = project.type_table.clone();
    // All synthesized helpers live under the entry module so the WIR-build
    // function registration picks them up: `register_loaded_functions` skips
    // anything in a `wasi:` module, which would orphan helpers for types
    // declared in WASI interfaces (e.g. `wasi:http/types`).
    let helper_module = project.entry_module_source.clone();
    let mut name_for_type: IndexMap<TypeId, (ModuleSource, String)> = IndexMap::default();
    let mut new_funcs: Vec<Rc<RefCell<TirFunction>>> = Vec::new();

    for type_id in copy_types {
        let func = generate_copy_function(type_id, project, &type_table_rc, &helper_module);
        name_for_type.insert(type_id, (func.module_source.clone(), func.name.clone()));
        new_funcs.push(Rc::new(RefCell::new(func)));
    }

    let mut visitor = RewriteCalls {
        name_for_type: &name_for_type,
    };
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            visitor.visit_block(body);
        }
    }

    project.functions.extend(new_funcs);
}

fn collect_copy_value_types(project: &FlatPackage) -> IndexSet<TypeId> {
    let mut collector = Collector {
        out: IndexSet::default(),
    };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(ref body) = func.body {
            collector.visit_block(body);
        }
    }
    collector.out
}

struct Collector {
    out: IndexSet<TypeId>,
}

impl TirRefVisitor for Collector {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let Some(t) = copy_value_type_arg(expr) {
            self.out.insert(t);
        }
        self.walk_expr(expr);
    }
}

fn copy_value_type_arg(expr: &TirExpr) -> Option<TypeId> {
    if let TirExprKind::Call { func, .. } = &expr.kind
        && func.module_source.is_core_builtin()
        && func.name == "copy_value"
    {
        func.monomorph_info
            .as_ref()
            .and_then(|mi| mi.impl_type_args.first().copied())
    } else {
        None
    }
}

fn dummy_span() -> Span {
    Span::new(0, 0, 1, 1)
}

fn generate_copy_function(
    type_id: TypeId,
    project: &FlatPackage,
    type_table: &Rc<RefCell<TypeTable>>,
    helper_module: &ModuleSource,
) -> TirFunction {
    let resolved = type_table.borrow().get(type_id).clone();
    let module_source = helper_module.clone();
    let span = dummy_span();
    let name = format!("$value_copy$T{}", type_id.0);

    let v_local = TirExpr::new(
        TirExprKind::Local {
            index: 0,
            name: "v".to_string(),
        },
        type_id,
        span,
    );

    let return_value = build_copy_body(type_id, &resolved, &v_local, project, type_table, span);
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(return_value),
            },
            span,
        )],
        span,
    );

    let param = TirParam {
        name: "v".to_string(),
        type_id,
        local_index: 0,
        is_mut: false,
        span,
        default_expr: None,
    };

    TirFunction {
        name,
        module_source,
        is_pub: false,
        is_export: false,
        is_async: false,
        type_params: vec![],
        impl_type_params: vec![],
        monomorph_info: None,
        method_info: None,
        params: vec![param],
        return_type: type_id,
        task_return_type: None,
        effects: vec![],
        stores: vec![],
        body: Some(body),
        span,
        local_count: 1,
        local_types: vec![type_id],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::ValueCopy { type_id },
    }
}

fn build_copy_body(
    type_id: TypeId,
    resolved: &ResolvedType,
    v_local: &TirExpr,
    project: &FlatPackage,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    if !matches!(
        resolved,
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
    ) {
        return v_local.clone();
    }
    let mangled = type_table.borrow().mangle_type_name(type_id);
    if let Some(struct_def) = lookup_struct(project, &mangled) {
        return build_struct_copy(type_id, &mangled, struct_def, v_local, type_table, span);
    }
    // GenericInstance whose monomorphized struct didn't get materialised in
    // `project.structs` (e.g., types reachable only as type references):
    // synthesize a body from the type-table-resolved field list. Fall back
    // to identity for variants and other shapes that have no field metadata.
    if let ResolvedType::GenericInstance { type_args, .. } = resolved
        && let Some(generic_struct) = generic_template_for(resolved, project)
    {
        return build_struct_copy_with_substitution(
            type_id,
            &mangled,
            generic_struct,
            type_args,
            v_local,
            type_table,
            span,
        );
    }
    // `Array<T>` wrapper structs are synthesized inside `wir_build` and never
    // surface as `TirStruct` entries; reconstruct the well-known shape from
    // the element type so we can still emit a deep-copy body for them.
    if let ResolvedType::GenericInstance {
        name, type_args, ..
    } = resolved
        && name == "Array"
        && type_args.len() == 1
    {
        return build_array_wrapper_copy(
            type_id,
            &mangled,
            type_args[0],
            v_local,
            type_table,
            span,
        );
    }
    // Tuples are likewise synthesized at WIR build with positional fields
    // `0`, `1`, …; reconstruct the body from the type-arg list.
    if let ResolvedType::GenericInstance {
        name,
        module_source,
        type_args,
    } = resolved
        && TypeTable::is_tuple_type(name, module_source)
    {
        return build_tuple_copy(type_id, &mangled, type_args, v_local, type_table, span);
    }
    v_local.clone()
}

fn lookup_struct<'a>(project: &'a FlatPackage, mangled_name: &str) -> Option<&'a TirStruct> {
    project.structs.iter().find(|s| s.name == mangled_name)
}

fn generic_template_for<'a>(
    resolved: &ResolvedType,
    project: &'a FlatPackage,
) -> Option<&'a TirStruct> {
    if let ResolvedType::GenericInstance {
        name,
        module_source,
        ..
    } = resolved
    {
        project
            .structs
            .iter()
            .find(|s| &s.name == name && &s.module_source == module_source)
    } else {
        None
    }
}

fn build_array_wrapper_copy(
    type_id: TypeId,
    mangled_name: &str,
    elem_type: TypeId,
    v_local: &TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let raw_array_ty = type_table.borrow_mut().make_builtin_array(elem_type);
    let repr_field = TirField {
        name: "repr".to_string(),
        is_pub: false,
        type_id: raw_array_ty,
        index: 0,
        span,
        is_hidden: false,
        serde_rename: None,
        serde_default: false,
        default_expr: None,
    };
    let used_field = TirField {
        name: "used".to_string(),
        is_pub: false,
        type_id: TypeTable::I32,
        index: 1,
        span,
        is_hidden: false,
        serde_rename: None,
        serde_default: false,
        default_expr: None,
    };
    let fields = vec![
        TirStructField {
            name: "repr".to_string(),
            value: make_field_copy(v_local.clone(), &repr_field, type_table, span),
            field_index: 0,
        },
        TirStructField {
            name: "used".to_string(),
            value: make_field_copy(v_local.clone(), &used_field, type_table, span),
            field_index: 1,
        },
    ];
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: type_id,
            struct_name: mangled_name.to_string(),
            fields,
        },
        type_id,
        span,
    )
}

fn build_tuple_copy(
    type_id: TypeId,
    mangled_name: &str,
    elem_types: &[TypeId],
    v_local: &TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let fields: Vec<TirStructField> = elem_types
        .iter()
        .enumerate()
        .map(|(idx, elem_ty)| {
            let field = TirField {
                name: idx.to_string(),
                is_pub: true,
                type_id: *elem_ty,
                index: idx as u32,
                span,
                is_hidden: false,
                serde_rename: None,
                serde_default: false,
                default_expr: None,
            };
            TirStructField {
                name: field.name.clone(),
                value: make_field_copy(v_local.clone(), &field, type_table, span),
                field_index: idx as u32,
            }
        })
        .collect();
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: type_id,
            struct_name: mangled_name.to_string(),
            fields,
        },
        type_id,
        span,
    )
}

fn build_struct_copy_with_substitution(
    type_id: TypeId,
    mangled_name: &str,
    generic_struct: &TirStruct,
    type_args: &[TypeId],
    v_local: &TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let mut substitution = crate::hashmap::IndexMap::default();
    for (idx, ty) in type_args.iter().enumerate() {
        substitution.insert(idx as u32, *ty);
    }
    let fields: Vec<TirStructField> = generic_struct
        .fields
        .iter()
        .map(|field| {
            let concrete_field_ty = type_table
                .borrow_mut()
                .substitute_type_params(field.type_id, &substitution);
            let concrete_field = TirField {
                name: field.name.clone(),
                is_pub: field.is_pub,
                type_id: concrete_field_ty,
                index: field.index,
                span: field.span,
                is_hidden: field.is_hidden,
                serde_rename: field.serde_rename.clone(),
                serde_default: field.serde_default,
                default_expr: field.default_expr.clone(),
            };
            let value = make_field_copy(v_local.clone(), &concrete_field, type_table, span);
            TirStructField {
                name: field.name.clone(),
                value,
                field_index: field.index,
            }
        })
        .collect();
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: type_id,
            struct_name: mangled_name.to_string(),
            fields,
        },
        type_id,
        span,
    )
}

fn build_struct_copy(
    type_id: TypeId,
    struct_name: &str,
    struct_def: &TirStruct,
    v_local: &TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let fields: Vec<TirStructField> = struct_def
        .fields
        .iter()
        .map(|field| {
            let value = make_field_copy(v_local.clone(), field, type_table, span);
            TirStructField {
                name: field.name.clone(),
                value,
                field_index: field.index,
            }
        })
        .collect();
    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: type_id,
            struct_name: struct_name.to_string(),
            fields,
        },
        type_id,
        span,
    )
}

fn make_field_copy(
    receiver: TirExpr,
    field: &TirField,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let field_access = TirExpr::new(
        TirExprKind::FieldAccess {
            expr: Box::new(receiver),
            field_index: field.index,
            field_name: field.name.clone(),
        },
        field.type_id,
        span,
    );
    let elem_type_opt = match type_table.borrow().get(field.type_id) {
        ResolvedType::BuiltinArray(elem) => Some(*elem),
        _ => None,
    };
    if let Some(elem_type) = elem_type_opt {
        let array_clone_ref = FunctionRef {
            module_source: ModuleSource::builtin(),
            name: "array_clone".to_string(),
            monomorph_info: Some(MonomorphInfo {
                generic_name: "array_clone".to_string(),
                impl_type_args: vec![elem_type],
                method_type_args: vec![],
                is_blanket: false,
            }),
            method_info: None,
        };
        TirExpr::new(
            TirExprKind::Call {
                func: array_clone_ref,
                type_args: vec![elem_type],
                args: vec![CallArg::new(field_access, false)],
            },
            field.type_id,
            span,
        )
    } else {
        field_access
    }
}

struct RewriteCalls<'a> {
    name_for_type: &'a IndexMap<TypeId, (ModuleSource, String)>,
}

impl TirOptVisitor for RewriteCalls<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        opt_walk_expr(self, expr);
        if let TirExprKind::Call {
            func,
            args,
            type_args,
        } = &mut expr.kind
            && func.module_source.is_core_builtin()
            && func.name == "copy_value"
            && let Some(type_id) = func
                .monomorph_info
                .as_ref()
                .and_then(|mi| mi.impl_type_args.first().copied())
            && let Some((module_source, name)) = self.name_for_type.get(&type_id)
            && args.len() == 1
        {
            *func = FunctionRef {
                module_source: module_source.clone(),
                name: name.clone(),
                monomorph_info: None,
                method_info: None,
            };
            *type_args = vec![];
        }
        false
    }

    fn visit_block(&mut self, block: &mut TirBlock) -> bool {
        opt_walk_block(self, block)
    }
}
