//! Generate `$value_copy$` helper functions for every type that the
//! fold needs at a wrap site, plus the transitive closure of nested
//! value-typed fields. The seed comes from [`super::analyze::collect_seed_types`];
//! the loop in [`synthesize_helpers`] iterates until no new types appear
//! in newly-generated helper bodies.
//!
//! For struct types the body is a `StructLiteral` with field-by-field
//! shallow projections, plus `builtin::array_clone::<T>` for raw
//! `Array<T>` fields. For variant types it is a `match` re-constructing
//! the matched case with a copied payload. Fall-through types
//! (references, resources) keep `return v;` (identity). Nested copies
//! are calls to the nested type's own helper, so the helpers are
//! mutually recursive.
//!
//! Synthesized helper bodies contain `builtin::copy_value::<NestedT>(x)`
//! markers for nested value-typed fields. The translator rewrites those
//! markers via [`crate::lower::translate`]'s `convert_call` arm. User
//! function bodies, in contrast, get the wrap call emitted directly by
//! the fold — they carry no markers after Phase A of WEP 2026-05-11
//! Step 5.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::SeqField;
use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType, TirBlock, TirExpr,
    TirExprKind, TirField, TirFunction, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirStructField, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;
use crate::token::Span;

use super::needs_value_copy;

pub fn synthesize_helpers(
    project: &mut FlatPackage,
    initial: IndexSet<TypeId>,
) -> IndexMap<TypeId, (ModuleSource, String)> {
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
    // The type table can intern the same structural type under more than one
    // `TypeId`, which would otherwise mint two identically-named helpers and
    // collide in the function registry. Helpers are identified by their
    // canonical mangle, so a second `TypeId` for an already-generated type
    // maps onto the same helper (recorded in `name_for_type`) instead of
    // producing a duplicate.
    let mut generated_names: IndexSet<String> = IndexSet::default();
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
        if generated_names.insert(func.name.clone()) {
            new_funcs.push(Rc::new(RefCell::new(func)));
        }
    }

    project.functions.extend(new_funcs);
    name_for_type
}

/// Walk a newly-generated helper body and collect every `TypeId` that
/// appears as a nested `copy_value::<NestedT>` marker (so the worklist
/// closes the transitive set of helpers) and every `array_clone::<T>`
/// element type (so codegen's per-element `$value_copy$T` call finds a
/// registered helper at lower time).
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
        // `$value_copy$` call inside the clone loop so each
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
        && crate::tir::matches_builtin(&func.name, func.monomorph_info.as_ref(), "copy_value")
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
        && crate::tir::matches_builtin(&func.name, func.monomorph_info.as_ref(), "array_clone")
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
    let name =
        crate::name::value_copy_helper_name(&type_table.borrow().mangle_type_arg_for_generic(type_id));

    let v_local = TirExpr::new(
        TirExprKind::Local {
            index: 0,
            name: "v".to_string(),
        },
        type_id,
        span,
    );

    let mut extra_locals: Vec<TirLocal> = Vec::new();
    let body = build_copy_body(
        type_id,
        &resolved,
        &v_local,
        project,
        type_table,
        span,
        &mut extra_locals,
    );

    let param = TirParam {
        name: "v".to_string(),
        type_id,
        local_index: 0,
        is_mut: false,
        is_mut_ref: false,
        span,
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
        visibility: crate::ast::Visibility::Private,
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
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        benign_effects: Vec::new(),
        inline_hint: InlineHint::Auto,
        compiler_item: None,
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
) -> TirBlock {
    // A bare `Array<T>` deep-copies via `array_clone::<T>(&v)`,
    // the same intrinsic `make_field_copy` emits for the `repr` field of
    // `List<T>` / `String`. This is what gives the raw array value
    // semantics (WEP-2026-06-02 Phase 2).
    if let ResolvedType::BuiltinArray(elem_type) = resolved {
        let clone = build_array_clone(v_local.clone(), type_id, *elem_type, type_table, span);
        return single_return_block(clone, span);
    }
    if let Some(cases) = variant_cases_concrete(resolved, type_table) {
        return build_variant_copy_body(type_id, &cases, v_local, type_table, span, extra_locals);
    }
    let return_expr = build_copy_return_expr(type_id, resolved, v_local, project, type_table, span);
    if let Some(expr) = return_expr {
        return single_return_block(expr, span);
    }
    single_return_block(v_local.clone(), span)
}

/// Concrete `(case name, case index, payload type)` list for a variant
/// type; `None` when `resolved` is not a variant. Reads the same case
/// templates `needs_value_copy` classifies from, so classification and
/// synthesis cannot drift.
fn variant_cases_concrete(
    resolved: &ResolvedType,
    type_table: &Rc<RefCell<TypeTable>>,
) -> Option<Vec<(String, u32, TypeId)>> {
    match resolved {
        ResolvedType::Variant {
            name,
            module_source,
        } => type_table
            .borrow()
            .variant_template_cases(name, module_source)
            .map(<[_]>::to_vec),
        ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        } => {
            let cases = type_table
                .borrow()
                .variant_template_cases(name, module_source)
                .map(<[_]>::to_vec)?;
            let mut substitution = crate::hashmap::IndexMap::default();
            for (idx, ty) in type_args.iter().enumerate() {
                substitution.insert(idx as u32, *ty);
            }
            Some(
                cases
                    .into_iter()
                    .map(|(case_name, index, payload)| {
                        let payload = type_table
                            .borrow_mut()
                            .substitute_type_params(payload, &substitution);
                        (case_name, index, payload)
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Match each case and re-construct it, routing value-typed payloads
/// through their own `$value_copy$T` helpers. A payload-free case is
/// immutable, so its arm returns the input unchanged.
fn build_variant_copy_body(
    type_id: TypeId,
    cases: &[(String, u32, TypeId)],
    v_local: &TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
    extra_locals: &mut Vec<TirLocal>,
) -> TirBlock {
    let arms: Vec<TirMatchArm> = cases
        .iter()
        .map(|(case_name, case_index, payload_ty)| {
            let (bindings, body) = if *payload_ty == TypeTable::UNIT {
                (vec![], v_local.clone())
            } else {
                // Local 0 is the `v` parameter.
                let local_index =
                    u32::try_from(extra_locals.len() + 1).expect("local index fits u32");
                let local_name = format!("__vc_payload_{case_index}");
                extra_locals.push(TirLocal {
                    name: local_name.clone(),
                    type_id: *payload_ty,
                    is_mut: false,
                });
                let payload_local = TirExpr::new(
                    TirExprKind::Local {
                        index: local_index,
                        name: local_name.clone(),
                    },
                    *payload_ty,
                    span,
                );
                let construct = TirExpr::new(
                    TirExprKind::VariantConstruct {
                        variant_type: type_id,
                        case_index: *case_index,
                        case_name: case_name.clone(),
                        payload: Some(Box::new(make_value_copy(
                            payload_local,
                            *payload_ty,
                            type_table,
                            span,
                        ))),
                    },
                    type_id,
                    span,
                );
                (
                    vec![TirPattern::Binding {
                        name: local_name,
                        local_index,
                        type_id: *payload_ty,
                    }],
                    construct,
                )
            };
            TirMatchArm {
                pattern: TirPattern::Variant {
                    enum_type: type_id,
                    variant_name: case_name.clone(),
                    bindings,
                    payload_type: *payload_ty,
                },
                guard: None,
                body,
                span,
            }
        })
        .collect();
    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(v_local.clone()),
            arms,
        },
        type_id,
        span,
    );
    single_return_block(match_expr, span)
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
    // Two same-named structs from distinct modules mangle to the same name
    // (`mangle_type_name` intentionally does not module-qualify structs), so the
    // copy body must be built from *this* type's module, not the first match.
    let module = match resolved {
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::GenericInstance { module_source, .. } => Some(module_source.clone()),
        _ => None,
    };
    let mangled = type_table.borrow().mangle_type_name(type_id);
    if let Some(struct_def) = lookup_struct(project, &mangled, module.as_ref()) {
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
    // (List<T> / tuple) is recovered by the explicit checks below.
    let list_name = type_table
        .borrow()
        .compiler_struct_name(crate::compiler_item::CompilerItem::List)
        .to_string();
    if let ResolvedType::GenericInstance {
        name, type_args, ..
    } = resolved
        && name == &list_name
        && type_args.len() == 1
        && is_synth_safe_element(type_args[0], type_table, project)
    {
        return Some(build_list_wrapper_copy(
            type_id,
            &mangled,
            type_args[0],
            v_local,
            type_table,
            span,
        ));
    }
    if let ResolvedType::GenericInstance {
        name, type_args, ..
    } = resolved
        && TypeTable::is_tuple_type(name)
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

/// Find the `TirStruct` for a mangled name. When several structs share the name
/// (same-named types from distinct modules), `module` disambiguates; a unique
/// name resolves without it, preserving behaviour for the common case.
fn lookup_struct<'a>(
    project: &'a FlatPackage,
    mangled_name: &str,
    module: Option<&ModuleSource>,
) -> Option<&'a TirStruct> {
    let mut named = project.structs.iter().filter(|s| s.name == mangled_name);
    let first = named.next()?;
    if named.next().is_none() {
        return Some(first);
    }
    match module {
        Some(m) => project
            .structs
            .iter()
            .find(|s| s.name == mangled_name && &s.module_source == m),
        None => Some(first),
    }
}

/// `List<T>`'s wrapper-struct deep-copy emits a `StructLiteral`
/// whose `type_id` is `List<T>`. WIR-level resolution of that
/// `type_id` becomes `AbstractRef(Struct)` whenever `T` is a shape
/// that the WIR struct registry never materialises a concrete entry
/// for — resources, function/closure types, and unmaterialised
/// generic instances. The downstream `StructLiteral` translation
/// only knows how to handle a concrete `WirType::Ref`, so we skip
/// the non-identity body and fall through to identity for those `T`s.
/// Built-in primitives, ordinary structs, variants, and known wrapper
/// shapes (`List<T>`, `String`, tuples) all have concrete WIR
/// registrations and are passed through.
fn is_synth_safe_element(
    elem_type: TypeId,
    type_table: &Rc<RefCell<TypeTable>>,
    project: &FlatPackage,
) -> bool {
    let resolved = type_table.borrow().get(elem_type).clone();
    match resolved {
        ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. }
        | ResolvedType::Function { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::TypePack { .. } => false,
        ResolvedType::Variant { .. } => true,
        ResolvedType::GenericInstance {
            name,
            module_source,
            ..
        } => {
            // Tuples / String / List<T> / known struct templates are
            // safe; unknown generic-instance names whose template
            // isn't a registered struct or variant are not.
            if TypeTable::is_tuple_type(&name) {
                return true;
            }
            let (list_name, string_name, box_name) = {
                let tt = type_table.borrow();
                let items = tt.compiler_items();
                (
                    items
                        .struct_name(crate::compiler_item::CompilerItem::List)
                        .to_string(),
                    items
                        .struct_name(crate::compiler_item::CompilerItem::String)
                        .to_string(),
                    items
                        .struct_name(crate::compiler_item::CompilerItem::Box)
                        .to_string(),
                )
            };
            if name == list_name || name == string_name || name == box_name {
                return true;
            }
            // A concrete monomorphised struct entry is the strongest
            // signal — without it WIR has no `Ref` to point at.
            let mangled = type_table.borrow().mangle_type_name(elem_type);
            if lookup_struct(project, &mangled, Some(&module_source)).is_some() {
                return true;
            }
            project.find_variant(&module_source, &name).is_some()
        }
        _ => true,
    }
}

fn build_list_wrapper_copy(
    type_id: TypeId,
    mangled_name: &str,
    elem_type: TypeId,
    v_local: &TirExpr,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let raw_array_ty = type_table.borrow_mut().make_builtin_array(elem_type);
    let repr_field = TirField {
        name: SeqField::Backing.field_name().to_string(),
        visibility: crate::ast::Visibility::Private,
        type_id: raw_array_ty,
        index: 0,
        span,
        is_secret: false,
        serde_rename: None,
        serde_default: false,
        serde_positional: false,
        default_expr: None,
    };
    let used_field = TirField {
        name: SeqField::Len.field_name().to_string(),
        visibility: crate::ast::Visibility::Private,
        type_id: TypeTable::I32,
        index: 1,
        span,
        is_secret: false,
        serde_rename: None,
        serde_default: false,
        serde_positional: false,
        default_expr: None,
    };
    let fields = vec![
        TirStructField {
            name: SeqField::Backing.field_name().to_string(),
            value: make_field_copy(v_local.clone(), &repr_field, type_table, span),
            field_index: 0,
        },
        TirStructField {
            name: SeqField::Len.field_name().to_string(),
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
                visibility: crate::ast::Visibility::Public,
                type_id: *elem_ty,
                index: idx as u32,
                span,
                is_secret: false,
                serde_rename: None,
                serde_default: false,
                serde_positional: false,
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
/// gets resolved to the `$value_copy$` helper by
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

/// Build `builtin::array_clone::<elem_type>(&arr)`, the intrinsic that
/// deep-copies a raw GC array. `array_ty` is the `Array<elem_type>`
/// type of `arr`; the call returns a fresh array of the same type. Codegen
/// gives the clone a per-element `$value_copy$T` pass when `elem_type` is
/// itself value-semantic (see `wir_build::calls::array_element_copy_func`).
fn build_array_clone(
    arr: TirExpr,
    array_ty: TypeId,
    elem_type: TypeId,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
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
    let ref_type = type_table.borrow_mut().make_ref(array_ty);
    let arr_ref = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(arr),
        },
        ref_type,
        span,
    );
    TirExpr::new(
        TirExprKind::Call {
            func: array_clone_ref,
            type_args: vec![elem_type],
            args: vec![CallArg::new(arr_ref, false)],
        },
        array_ty,
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
    make_value_copy(field_access, field.type_id, type_table, span)
}

/// Route an already-projected value through the deep-copy machinery: a
/// raw `Array<T>` via `array_clone`, a value-semantic type via its
/// `$value_copy$T` helper (a `builtin::copy_value::<T>(...)` marker the
/// TIR → NIR translator resolves), everything else as-is.
fn make_value_copy(
    expr: TirExpr,
    type_id: TypeId,
    type_table: &Rc<RefCell<TypeTable>>,
    span: Span,
) -> TirExpr {
    let resolved = type_table.borrow().get(type_id).clone();
    if let ResolvedType::BuiltinArray(elem_type) = resolved {
        return build_array_clone(expr, type_id, elem_type, type_table, span);
    }
    if needs_value_copy(type_id, &type_table.borrow()) {
        return wrap_copy_value(expr, type_id, span);
    }
    expr
}
