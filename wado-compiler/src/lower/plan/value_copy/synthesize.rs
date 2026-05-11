//! Generate `$value_copy$T<id>` helper functions for every type that the
//! marker pass [`super::insert::insert_value_copy_calls`] has wrapped in
//! `builtin::copy_value::<T>(x)`. Pushes the helpers into
//! [`FlatPackage::functions`] and returns the
//! `TypeId → (ModuleSource, name)` map.
//!
//! For struct types the body is a `StructLiteral` with field-by-field
//! shallow projections, plus `builtin::array_clone::<T>` for raw
//! `builtin::array<T>` fields — a one-level shallow copy that does not
//! recurse into nested aggregates. For variant / option / fall-through
//! types the body is `return v;` (identity).
//!
//! Rewriting the `builtin::copy_value::<T>(x)` markers into calls to the
//! generated helpers is the translator's job (see
//! [`crate::lower::translate`]). Both user-function bodies and the
//! synthesized helper bodies (which themselves contain
//! `copy_value::<NestedT>(...)` for nested value-typed fields) flow
//! through the same TIR → NIR fold and pick up the rewrite uniformly.

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType, TirBlock, TirExpr,
    TirExprKind, TirField, TirFunction, TirLocal, TirParam, TirStmt, TirStmtKind, TirStruct,
    TirStructField, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;

use super::needs_value_copy;

pub fn synthesize_helpers(project: &mut FlatPackage) -> IndexMap<TypeId, (ModuleSource, String)> {
    let initial = collect_copy_value_types(project);
    if initial.is_empty() {
        return IndexMap::default();
    }

    let type_table_rc = project.type_table.clone();
    // All synthesized helpers live under the entry module so the WIR-build
    // function registration picks them up: `register_loaded_functions` skips
    // anything in a `wasi:` module, which would orphan helpers for types
    // declared in WASI interfaces (e.g. `wasi:http/types`).
    let helper_module = project.entry_module_source.clone();
    let mut name_for_type: IndexMap<TypeId, (ModuleSource, String)> = IndexMap::default();
    let mut new_funcs: Vec<Rc<RefCell<TirFunction>>> = Vec::new();

    // Worklist over the transitive closure of types that need a
    // `$value_copy$T` helper. `make_field_copy` emits
    // `copy_value::<FieldT>(...)` for nested value-semantic fields, so
    // generating one helper can surface new types whose own helpers we
    // also need. Iterate until no new types appear.
    let mut seen: IndexSet<TypeId> = initial.iter().copied().collect();
    let mut worklist: Vec<TypeId> = initial.into_iter().collect();
    while let Some(type_id) = worklist.pop() {
        let func = generate_copy_function(type_id, project, &type_table_rc, &helper_module);
        if let Some(ref body) = func.body {
            let tt = type_table_rc.borrow();
            let mut sub = Collector {
                out: IndexSet::default(),
                type_table: &tt,
            };
            sub.visit_block(body);
            for nested in sub.out {
                if seen.insert(nested) {
                    worklist.push(nested);
                }
            }
        }
        name_for_type.insert(type_id, (func.module_source.clone(), func.name.clone()));
        new_funcs.push(Rc::new(RefCell::new(func)));
    }

    project.functions.extend(new_funcs);
    name_for_type
}

fn collect_copy_value_types(project: &FlatPackage) -> IndexSet<TypeId> {
    let type_table = project.type_table.borrow();
    let mut collector = Collector {
        out: IndexSet::default(),
        type_table: &type_table,
    };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(ref body) = func.body {
            collector.visit_block(body);
        }
    }
    collector.out
}

struct Collector<'a> {
    out: IndexSet<TypeId>,
    type_table: &'a TypeTable,
}

impl TirRefVisitor for Collector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let Some(t) = copy_value_type_arg(expr) {
            self.out.insert(t);
        }
        // `array_clone::<T>(arr)` lowers to a `WirInstr::ArrayClone`.
        // When `T` is a value-typed struct, codegen emits a per-element
        // `$value_copy$T<id>` call inside the clone loop so each
        // destination element is a fresh struct rather than an aliased
        // ref. The helper has to actually exist by then, so collect
        // such element types into the worklist alongside direct
        // `copy_value::<T>` callers.
        if let Some(t) = array_clone_element_type_arg(expr)
            && super::needs_value_copy(t, self.type_table)
        {
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

fn array_clone_element_type_arg(expr: &TirExpr) -> Option<TypeId> {
    if let TirExprKind::Call { func, .. } = &expr.kind
        && func.module_source.is_core_builtin()
        && func.name == "array_clone"
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

    let mut extra_locals: Vec<TirLocal> = Vec::new();
    let mut address_taken: IndexSet<u32> = IndexSet::default();
    let body = build_copy_body(
        type_id,
        &resolved,
        &v_local,
        project,
        type_table,
        span,
        &mut extra_locals,
        &mut address_taken,
    );

    let param = TirParam {
        name: "v".to_string(),
        type_id,
        local_index: 0,
        is_mut: false,
        span,
        default_expr: None,
    };

    let mut locals = vec![TirLocal {
        name: "v".to_string(),
        type_id,
        is_mut: false,
    }];
    locals.extend(extra_locals);
    let local_count = u32::try_from(locals.len()).expect("local count fits in u32");

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
        local_count,
        locals,
        address_taken_locals: address_taken,
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

        return_abi: crate::tir::ReturnAbi::default(),
    }
}

fn build_copy_body(
    type_id: TypeId,
    resolved: &ResolvedType,
    v_local: &TirExpr,
    project: &FlatPackage,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
    extra_locals: &mut Vec<TirLocal>,
    address_taken: &mut IndexSet<u32>,
) -> TirBlock {
    let return_expr = build_copy_return_expr(type_id, resolved, v_local, project, type_table, span);
    if let Some(expr) = return_expr {
        return single_return_block(expr, span);
    }
    // Variant payload deep-copy (`Option<T>` / `Result<T, E>` /
    // user-defined variants) is intentionally not synthesized here —
    // see `super::needs_value_copy` for why a non-identity match body
    // hits a `(ref X) / (ref null X)` validation mismatch on null
    // sources.
    let _ = (extra_locals, address_taken, type_table);
    single_return_block(v_local.clone(), span)
}

/// Identity / struct-literal helpers that produce a single expression
/// to be returned. Returns `None` for the `Ref` / `MutRef` /
/// `Option<T>` / `Result<T, E>` cases that need a richer body.
fn build_copy_return_expr(
    type_id: TypeId,
    resolved: &ResolvedType,
    v_local: &TirExpr,
    project: &FlatPackage,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> Option<TirExpr> {
    if !matches!(
        resolved,
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
    ) {
        return None;
    }
    let mangled = type_table.borrow().mangle_type_name(type_id);
    if let Some(struct_def) = lookup_struct(project, &mangled)
        && !struct_has_recursive_variant(type_id, type_table, project)
    {
        return Some(build_struct_copy(
            type_id, &mangled, struct_def, v_local, type_table, span,
        ));
    }
    // `GenericInstance` whose monomorphized struct didn't get
    // materialised in `project.structs` is an unresolved-at-WIR-level
    // shape — its `type_id` resolves to `AbstractRef(Struct)` rather
    // than a concrete `WirType::Ref`, which the WIR-build
    // `StructLiteral` arm rejects with a hard panic. Falling through
    // here without a non-identity body keeps every reachable
    // `array_clone::<T>` generation safe even when a stray reachable
    // type in the closure happens to be a non-monomorphized template
    // wrapper. The shape that *does* need a non-identity copy
    // (Array<T> / tuple) is recovered by the explicit checks below.
    if let ResolvedType::GenericInstance {
        name, type_args, ..
    } = resolved
        && name == "Array"
        && type_args.len() == 1
        && is_synth_safe_element(type_args[0], type_table, project)
    {
        return Some(build_array_wrapper_copy(
            type_id,
            &mangled,
            type_args[0],
            v_local,
            type_table,
            span,
        ));
    }
    if let ResolvedType::GenericInstance {
        name,
        module_source,
        type_args,
    } = resolved
        && TypeTable::is_tuple_type(name, module_source)
    {
        return Some(build_tuple_copy(
            type_id, &mangled, type_args, v_local, type_table, span,
        ));
    }
    None
}

fn single_return_block(value: TirExpr, span: Span) -> TirBlock {
    TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return { value: Some(value) },
            span,
        )],
        span,
    )
}

fn lookup_struct<'a>(project: &'a FlatPackage, mangled_name: &str) -> Option<&'a TirStruct> {
    project.structs.iter().find(|s| s.name == mangled_name)
}

#[allow(dead_code)]
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

/// True when `type_id` is part of a struct cycle that runs through a
/// variant payload (e.g. `Object(TreeMap<String, Value>)` in the
/// recursive `Value` variant). Concrete WIR registration for these
/// cycles uses `AbstractRef(Struct)` placeholders that the
/// post-registration fix-up only resolves *inside* struct field
/// definitions — the `expr.type_id` on a synthesized helper's
/// `StructLiteral` body therefore can still resolve to `AbstractRef`
/// at WIR-translate time, hitting the hard panic in that arm.
/// Skipping the non-identity body for these types is sound: the
/// caller already routes through `$value_copy$T<id>` either way, and
/// identity is a strict subset of the value-semantic deep copy
/// behaviour the rest of the helper would have provided. The
/// underlying gap (per-position deep copy of cyclic recursive types)
/// is tracked as a follow-up.
fn struct_has_recursive_variant(
    type_id: TypeId,
    type_table: &Rc<RefCell<TypeTable>>,
    project: &FlatPackage,
) -> bool {
    let mut seen: IndexSet<TypeId> = IndexSet::default();
    contains_variant_recursive(type_id, type_table, project, &mut seen, 0)
}

fn contains_variant_recursive(
    type_id: TypeId,
    type_table: &Rc<RefCell<TypeTable>>,
    project: &FlatPackage,
    seen: &mut IndexSet<TypeId>,
    depth: usize,
) -> bool {
    if depth > 16 || !seen.insert(type_id) {
        return false;
    }
    let resolved = type_table.borrow().get(type_id).clone();
    match resolved {
        ResolvedType::Variant { .. } => true,
        ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        } => {
            // Generic-variant instance (`Option<T>` / `Result<T, E>` /
            // user-defined recursive variants like `Value` from
            // `core:json_value`): treat as a variant boundary, since
            // its monomorphised registration in WIR may still be an
            // `AbstractRef(Struct)` placeholder when the helper body
            // is translated.
            if type_table
                .borrow()
                .find_struct_type(&name, &module_source)
                .is_none()
            {
                return true;
            }
            // Concrete monomorphised struct — recurse into its fields
            // and type args.
            for arg in &type_args {
                if contains_variant_recursive(*arg, type_table, project, seen, depth + 1) {
                    return true;
                }
            }
            let mangled = type_table.borrow().mangle_type_name(type_id);
            if let Some(struct_def) = lookup_struct(project, &mangled) {
                let mut substitution = crate::hashmap::IndexMap::default();
                for (idx, ty) in type_args.iter().enumerate() {
                    substitution.insert(idx as u32, *ty);
                }
                for field in &struct_def.fields {
                    let concrete = type_table
                        .borrow_mut()
                        .substitute_type_params(field.type_id, &substitution);
                    if contains_variant_recursive(concrete, type_table, project, seen, depth + 1) {
                        return true;
                    }
                }
            }
            false
        }
        ResolvedType::Struct { .. } => {
            let mangled = type_table.borrow().mangle_type_name(type_id);
            if let Some(struct_def) = lookup_struct(project, &mangled) {
                for field in &struct_def.fields {
                    if contains_variant_recursive(
                        field.type_id,
                        type_table,
                        project,
                        seen,
                        depth + 1,
                    ) {
                        return true;
                    }
                }
            }
            false
        }
        ResolvedType::BuiltinArray(elem) => {
            contains_variant_recursive(elem, type_table, project, seen, depth + 1)
        }
        _ => false,
    }
}

/// `Array<T>`'s wrapper-struct deep-copy emits a `StructLiteral`
/// whose `type_id` is `Array<T>`. WIR-level resolution of that
/// `type_id` becomes `AbstractRef(Struct)` whenever `T` is a shape
/// that the WIR struct registry never materialises a concrete entry
/// for — variants, resources, function/closure types, and unmaterialised
/// generic instances. The downstream `StructLiteral` translation
/// only knows how to handle a concrete `WirType::Ref`, so we skip
/// the non-identity body and fall through to identity for those `T`s.
/// Built-in primitives, ordinary structs, and known wrapper shapes
/// (`Array<T>`, `String`, tuples) all have concrete WIR registrations
/// and are passed through.
fn is_synth_safe_element(
    elem_type: TypeId,
    type_table: &Rc<RefCell<TypeTable>>,
    project: &FlatPackage,
) -> bool {
    let resolved = type_table.borrow().get(elem_type).clone();
    match resolved {
        ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::Function { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. } => false,
        ResolvedType::GenericInstance {
            name,
            module_source,
            ..
        } => {
            // Tuples / String / Array<T> / known struct templates are
            // safe; unknown generic-instance names whose template
            // isn't a registered struct are not.
            if TypeTable::is_tuple_type(&name, &module_source) {
                return true;
            }
            if name == "Array" || name == "String" || name == "Box" {
                return true;
            }
            // A concrete monomorphised struct entry is the strongest
            // signal — without it WIR has no `Ref` to point at.
            let mangled = type_table.borrow().mangle_type_name(elem_type);
            if lookup_struct(project, &mangled).is_some() {
                return true;
            }
            // Generic-variant instances (`Option<T>` / `Result<T, E>` /
            // user-defined variants) are reference-shaped at WIR but
            // their helper bodies are identity, so the call site never
            // wraps a wrapper around them anyway. Treat as safe.
            if type_table
                .borrow()
                .find_struct_type(&name, &module_source)
                .is_none()
            {
                return false;
            }
            true
        }
        _ => true,
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

#[allow(dead_code)]
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

/// Wrap `expr` in `builtin::copy_value::<type_id>(expr)`. The wrapper
/// gets resolved to the `$value_copy$T<id>` helper by
/// [`crate::lower::translate`] when the TIR is folded into NIR.
fn wrap_copy_value(expr: TirExpr, type_id: TypeId, span: Span) -> TirExpr {
    let func = FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "copy_value".to_string(),
        monomorph_info: Some(MonomorphInfo {
            generic_name: "copy_value".to_string(),
            impl_type_args: vec![type_id],
            method_type_args: vec![],
            is_blanket: false,
        }),
        method_info: None,
    };
    TirExpr::new(
        TirExprKind::Call {
            func,
            type_args: vec![type_id],
            args: vec![CallArg::new(expr, false)],
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
    let resolved = type_table.borrow().get(field.type_id).clone();
    if let ResolvedType::BuiltinArray(elem_type) = resolved {
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
        return TirExpr::new(
            TirExprKind::Call {
                func: array_clone_ref,
                type_args: vec![elem_type],
                args: vec![CallArg::new(field_access, false)],
            },
            field.type_id,
            span,
        );
    }
    // Nested value-semantic field (struct, Array<T>, tuple, Option<T>
    // / Result<T, E> with deep-copy payload, MutRef<T>, etc.). Without
    // this wrap the generated body would do a one-level shallow copy
    // and the inner type's own deep-copy via its `$value_copy$T` helper
    // would never run — mutations through the copy would alias the
    // original. Emit `builtin::copy_value::<FieldT>(field)`;
    // The TIR → NIR translator resolves the marker to the helper that
    // [`generate_copy_function`] synthesizes for `FieldT`.
    if needs_value_copy(field.type_id, &type_table.borrow()) {
        return wrap_copy_value(field_access, field.type_id, span);
    }
    field_access
}
