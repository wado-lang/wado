//! `FlatPackage` → `NirPackage` conversion.
//!
//! Field-for-field reconstruction: TIR and NIR body types have identical
//! shapes (NIR is a renamed copy of TIR). See `docs/wep-2026-05-11-nir.md`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::nir;
use crate::nir::{
    NirBlock, NirCapture, NirEnum, NirEnumCase, NirExpr, NirExprKind, NirField, NirFlags,
    NirFlagsMember, NirFunction, NirGlobal, NirImport, NirLiteralPattern, NirLocal, NirMatchArm,
    NirParam, NirPattern, NirStmt, NirStmtKind, NirStruct, NirStructField, NirStructPatternField,
    NirTest, NirTypeParam, NirVariantCase, NirVariantDecl,
};
use crate::nir_package::NirPackage;
use crate::tir;
use crate::tir::{
    CallArg, ClosureFunctor, FunctionRef, MonomorphInfo, TirBlock, TirCapture, TirEnum,
    TirEnumCase, TirExpr, TirExprKind, TirField, TirFlags, TirFlagsMember, TirFunction, TirGlobal,
    TirImport, TirLiteralPattern, TirLocal, TirMatchArm, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirStructField, TirStructPatternField, TirTest, TirTypeParam,
    TirVariantCase, TirVariantDecl,
};

/// Convert a [`FlatPackage`] (TIR-shaped) into a [`NirPackage`] (NIR-shaped).
///
/// Takes ownership of `flat` so owned containers (`Vec`s, `IndexMap`s,
/// `String`s, `BuiltinRegistry`, the trait env, …) move straight into the
/// `NirPackage` instead of being cloned. Body-shape Vecs (`structs`,
/// `enums`, …) iterate by reference because the per-element conversion
/// helpers (`convert_struct`, …) take `&T` and clone the few owned
/// `String` fields they keep — a per-helper rewrite to fully-move plumbing
/// is out of scope here. Function `Rc`s are destructured via
/// `.borrow().clone()` because each one is shared with one or more
/// `ClosureFunctor::call_method`; the closure-functor conversion looks up
/// the fresh `NirFunction` `Rc` in `func_map` so the optimizer's
/// `Rc::ptr_eq`-based closure-type DCE pass keeps matching.
pub fn flat_to_nir(flat: FlatPackage) -> NirPackage {
    let FlatPackage {
        entry_module_source,
        type_table,
        functions,
        structs,
        enums,
        variants,
        variant_index,
        flags,
        globals,
        imports,
        tests,
        string_literals,
        bytes_literals,
        closure_functors,
        function_strings,
        function_method_info,
        wasm_module_sources,
        module_name,
        wasi_registry,
        world_registry,
        used_wasi_functions,
        strip_names,
        skip_validation,
        target_world,
        has_http_handler_export,
        export_binding_names,
        component_plan,
        builtin_registry,
        task_return_flat_params,
        wasm_assets,
        trait_env,
    } = flat;

    let mut func_map: IndexMap<*const RefCell<TirFunction>, Rc<RefCell<NirFunction>>> =
        IndexMap::with_capacity_and_hasher(functions.len(), rustc_hash::FxBuildHasher);
    let functions: Vec<Rc<RefCell<NirFunction>>> = functions
        .into_iter()
        .map(|func_rc| {
            let ptr = Rc::as_ptr(&func_rc);
            let nir_rc = Rc::new(RefCell::new(convert_function(&func_rc.borrow())));
            func_map.insert(ptr, Rc::clone(&nir_rc));
            nir_rc
        })
        .collect();
    NirPackage {
        entry_module_source,
        type_table,
        functions,
        structs: structs.iter().map(convert_struct).collect(),
        enums: enums.iter().map(convert_enum).collect(),
        variants: variants.iter().map(convert_variant_decl).collect(),
        variant_index,
        flags: flags.iter().map(convert_flags).collect(),
        globals: globals.iter().map(convert_global).collect(),
        imports: imports.iter().map(convert_import).collect(),
        tests: tests.iter().map(convert_test).collect(),
        string_literals,
        bytes_literals,
        closure_functors: closure_functors
            .iter()
            .map(|cf| convert_closure_functor(cf, &func_map))
            .collect(),
        function_strings,
        function_method_info,
        wasm_module_sources,
        module_name,
        wasi_registry,
        world_registry,
        used_wasi_functions,
        strip_names,
        skip_validation,
        target_world,
        has_http_handler_export,
        export_binding_names,
        component_plan,
        builtin_registry,
        task_return_flat_params,
        wasm_assets,
        trait_env,
    }
}

fn convert_function(func: &TirFunction) -> NirFunction {
    NirFunction {
        name: func.name.clone(),
        module_source: func.module_source.clone(),
        is_pub: func.is_pub,
        is_export: func.is_export,
        is_async: func.is_async,
        type_params: func.type_params.iter().map(convert_type_param).collect(),
        impl_type_params: func
            .impl_type_params
            .iter()
            .map(convert_type_param)
            .collect(),
        monomorph_info: func.monomorph_info.as_ref().map(convert_monomorph_info),
        method_info: func.method_info.clone(),
        params: func.params.iter().map(convert_param).collect(),
        return_type: func.return_type,
        task_return_type: func.task_return_type,
        effects: func.effects.clone(),
        stores: func.stores.clone(),
        body: func.body.as_ref().map(convert_block),
        span: func.span,
        local_count: func.local_count,
        locals: func.locals.iter().map(convert_local).collect(),
        address_taken_locals: func.address_taken_locals.clone(),
        stores_aliased_locals: func.stores_aliased_locals.clone(),
        is_cm_binding: func.is_cm_binding,
        is_dispatch_wrapper: func.is_dispatch_wrapper,
        is_cm_export: func.is_cm_export,
        is_ambient: func.is_ambient,
        inline_hint: convert_inline_hint(func.inline_hint),
        comp_features: func.comp_features,
        export_name: func.export_name.clone(),
        allocator_tag: func.allocator_tag.clone(),
        kind: convert_function_kind(&func.kind),
        return_abi: convert_return_abi(&func.return_abi),
    }
}

fn convert_global(global: &TirGlobal) -> NirGlobal {
    NirGlobal {
        name: global.name.clone(),
        ty: global.ty,
        initializer: convert_expr(&global.initializer),
        mutable: global.mutable,
        wado_mutable: global.wado_mutable,
        is_pub: global.is_pub,
        module_source: global.module_source.clone(),
        span: global.span,
        is_nullable: global.is_nullable,
        lazy_init: global.lazy_init,
        locals: global.locals.iter().map(convert_local).collect(),
    }
}

fn convert_test(test: &TirTest) -> NirTest {
    NirTest {
        name: test.name.clone(),
        function_name: test.function_name.clone(),
        line: test.line,
        span: test.span,
        expect_trap: test.expect_trap,
        is_todo: test.is_todo,
        timeout_ms: test.timeout_ms,
    }
}

fn convert_struct(s: &TirStruct) -> NirStruct {
    NirStruct {
        name: s.name.clone(),
        module_source: s.module_source.clone(),
        is_pub: s.is_pub,
        type_params: s.type_params.iter().map(convert_type_param).collect(),
        monomorph_info: s.monomorph_info.as_ref().map(convert_monomorph_info),
        fields: s.fields.iter().map(convert_field).collect(),
        span: s.span,
        serde_rename_all: s.serde_rename_all.clone(),
    }
}

fn convert_enum(e: &TirEnum) -> NirEnum {
    NirEnum {
        name: e.name.clone(),
        module_source: e.module_source.clone(),
        is_pub: e.is_pub,
        type_params: e.type_params.iter().map(convert_type_param).collect(),
        monomorph_info: e.monomorph_info.as_ref().map(convert_monomorph_info),
        cases: e.cases.iter().map(convert_enum_case).collect(),
        span: e.span,
    }
}

fn convert_flags(f: &TirFlags) -> NirFlags {
    NirFlags {
        name: f.name.clone(),
        module_source: f.module_source.clone(),
        is_pub: f.is_pub,
        type_id: f.type_id,
        members: f.members.iter().map(convert_flags_member).collect(),
        span: f.span,
    }
}

fn convert_variant_decl(v: &TirVariantDecl) -> NirVariantDecl {
    NirVariantDecl {
        name: v.name.clone(),
        module_source: v.module_source.clone(),
        is_pub: v.is_pub,
        type_params: v.type_params.iter().map(convert_type_param).collect(),
        cases: v.cases.iter().map(convert_variant_case).collect(),
        comp_features: v.comp_features,
        span: v.span,
    }
}

fn convert_import(i: &TirImport) -> NirImport {
    NirImport {
        namespace: i.namespace.clone(),
        canonical_name: i.canonical_name.clone(),
        func_name: i.func_name.clone(),
        params: i.params.clone(),
        return_type: i.return_type,
    }
}

fn convert_closure_functor(
    cf: &ClosureFunctor,
    func_map: &IndexMap<*const RefCell<TirFunction>, Rc<RefCell<NirFunction>>>,
) -> nir::ClosureFunctor {
    // Reuse the converted function `Rc` when the functor's `call_method`
    // shares its allocation with one of the package's top-level functions
    // (the common case). Fall back to a fresh conversion only if the
    // functor's call_method is not present in the package's function list.
    let call_method = func_map
        .get(&Rc::as_ptr(&cf.call_method))
        .cloned()
        .unwrap_or_else(|| Rc::new(RefCell::new(convert_function(&cf.call_method.borrow()))));
    nir::ClosureFunctor {
        module_source: cf.module_source.clone(),
        id: cf.id,
        struct_name: cf.struct_name.clone(),
        struct_type_id: cf.struct_type_id,
        ref_type_id: cf.ref_type_id,
        call_method,
        captures: cf.captures.iter().map(convert_capture).collect(),
        canonical_user_params: cf.canonical_user_params.clone(),
        canonical_return: cf.canonical_return,
    }
}

fn convert_block(block: &TirBlock) -> NirBlock {
    NirBlock {
        stmts: block.stmts.iter().map(convert_stmt).collect(),
        span: block.span,
    }
}

fn convert_stmt(stmt: &TirStmt) -> NirStmt {
    NirStmt {
        kind: convert_stmt_kind(&stmt.kind),
        span: stmt.span,
    }
}

fn convert_stmt_kind(kind: &TirStmtKind) -> NirStmtKind {
    match kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
            skip_value_copy,
        } => NirStmtKind::Let {
            name: name.clone(),
            local_index: *local_index,
            is_mut: *is_mut,
            is_reactive: *is_reactive,
            type_id: *type_id,
            value: convert_expr(value),
            skip_value_copy: *skip_value_copy,
        },
        TirStmtKind::Expr(expr) => NirStmtKind::Expr(convert_expr(expr)),
        TirStmtKind::Return { value } => NirStmtKind::Return {
            value: value.as_ref().map(convert_expr),
        },
        TirStmtKind::TaskReturn { .. } => unreachable!(
            "TirStmtKind::TaskReturn should be eliminated by synthesis::cm_binding before nir_convert::flat_to_nir runs"
        ),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => NirStmtKind::If {
            condition: convert_expr(condition),
            then_block: convert_block(then_block),
            else_block: else_block.as_ref().map(convert_block),
        },
        TirStmtKind::Loop { body } => NirStmtKind::Loop {
            body: convert_block(body),
        },
        TirStmtKind::Break { label, value } => NirStmtKind::Break {
            label: label.clone(),
            value: value.as_ref().map(convert_expr),
        },
        TirStmtKind::Continue => NirStmtKind::Continue,
        TirStmtKind::LabeledBlock { label, block } => NirStmtKind::LabeledBlock {
            label: label.clone(),
            block: convert_block(block),
        },
        TirStmtKind::IfLet { .. } => unreachable!(
            "TirStmtKind::IfLet should be lowered by lower::pattern before nir_convert::flat_to_nir runs"
        ),
        TirStmtKind::LetDestructure {
            pattern,
            is_mut,
            value,
        } => NirStmtKind::LetDestructure {
            pattern: convert_pattern(pattern),
            is_mut: *is_mut,
            value: convert_expr(value),
        },
        TirStmtKind::VariadicForOf { .. } => unreachable!(
            "TirStmtKind::VariadicForOf should be expanded by monomorphize before nir_convert::flat_to_nir runs"
        ),
    }
}

fn convert_expr(expr: &TirExpr) -> NirExpr {
    NirExpr {
        kind: convert_expr_kind(&expr.kind),
        type_id: expr.type_id,
        span: expr.span,
    }
}

fn convert_expr_kind(kind: &TirExprKind) -> NirExprKind {
    match kind {
        TirExprKind::IntLiteral { value, repr } => NirExprKind::IntLiteral {
            value: *value,
            repr: repr.clone(),
        },
        TirExprKind::FloatLiteral { value, repr } => NirExprKind::FloatLiteral {
            value: *value,
            repr: repr.clone(),
        },
        TirExprKind::BoolLiteral(b) => NirExprKind::BoolLiteral(*b),
        TirExprKind::CharLiteral(c) => NirExprKind::CharLiteral(*c),
        TirExprKind::StringLiteral(s) => NirExprKind::StringLiteral(s.clone()),
        TirExprKind::BytesLiteral(b) => NirExprKind::BytesLiteral(b.clone()),
        TirExprKind::Null => NirExprKind::Null,
        TirExprKind::Unit => NirExprKind::Unit,
        TirExprKind::Local { index, name } => NirExprKind::Local {
            index: *index,
            name: name.clone(),
        },
        TirExprKind::FuncRef { .. } => unreachable!(
            "TirExprKind::FuncRef should be wrapped in a Closure by lower::closure before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::GlobalVarGet {
            module_source,
            name,
        } => NirExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.clone(),
        },
        TirExprKind::GlobalVarSet {
            module_source,
            name,
            value,
        } => NirExprKind::GlobalVarSet {
            module_source: module_source.clone(),
            name: name.clone(),
            value: Box::new(convert_expr(value)),
        },
        TirExprKind::Binary { left, op, right } => NirExprKind::Binary {
            left: Box::new(convert_expr(left)),
            op: convert_binary_op(*op),
            right: Box::new(convert_expr(right)),
        },
        TirExprKind::Unary { op, expr } => NirExprKind::Unary {
            op: convert_unary_op(*op),
            expr: Box::new(convert_expr(expr)),
        },
        TirExprKind::Assign { target, value } => NirExprKind::Assign {
            target: Box::new(convert_expr(target)),
            value: Box::new(convert_expr(value)),
        },
        TirExprKind::Cast { expr, target_type } => NirExprKind::Cast {
            expr: Box::new(convert_expr(expr)),
            target_type: *target_type,
        },
        TirExprKind::Call {
            func,
            type_args,
            args,
        } => NirExprKind::Call {
            func: convert_function_ref(func),
            type_args: type_args.clone(),
            args: args.iter().map(convert_call_arg).collect(),
        },
        TirExprKind::CmRawCall { local_name, args } => NirExprKind::CmRawCall {
            local_name: local_name.clone(),
            args: args.iter().map(convert_expr).collect(),
        },
        TirExprKind::MethodCall {
            receiver,
            func,
            type_args,
            args,
            ..
        } => NirExprKind::method_call(
            Box::new(convert_expr(receiver)),
            convert_function_ref(func),
            type_args.clone(),
            args.iter().map(convert_call_arg).collect(),
        ),
        TirExprKind::FieldAccess {
            expr,
            field_index,
            field_name,
        } => NirExprKind::FieldAccess {
            expr: Box::new(convert_expr(expr)),
            field_index: *field_index,
            field_name: field_name.clone(),
        },
        TirExprKind::Index { expr, index } => NirExprKind::Index {
            expr: Box::new(convert_expr(expr)),
            index: Box::new(convert_expr(index)),
        },
        TirExprKind::Block(block) => NirExprKind::Block(convert_block(block)),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => NirExprKind::If {
            condition: Box::new(convert_expr(condition)),
            then_branch: convert_block(then_branch),
            else_branch: else_branch.as_ref().map(convert_block),
        },
        TirExprKind::Match { expr, arms } => NirExprKind::Match {
            expr: Box::new(convert_expr(expr)),
            arms: arms.iter().map(convert_match_arm).collect(),
        },
        TirExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } => NirExprKind::StructLiteral {
            struct_type: *struct_type,
            struct_name: struct_name.clone(),
            fields: fields.iter().map(convert_struct_field).collect(),
        },
        TirExprKind::TupleLiteral { elements } => NirExprKind::TupleLiteral {
            elements: elements.iter().map(convert_expr).collect(),
        },
        TirExprKind::TupleSpread { .. } => unreachable!(
            "TirExprKind::TupleSpread should be expanded by monomorphize before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::TupleZip { .. } => unreachable!(
            "TirExprKind::TupleZip should be expanded by monomorphize before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::TypePackExpansion { .. } => unreachable!(
            "TirExprKind::TypePackExpansion should be expanded by monomorphize before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::Capture { .. } => unreachable!(
            "TirExprKind::Capture should be lowered to FieldAccess by lower::closure before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::Closure { .. } => unreachable!(
            "TirExprKind::Closure should be lowered to StructLiteral/ClosureToCanonical by lower::closure before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::IndirectCall { callee, args } => NirExprKind::IndirectCall {
            callee: Box::new(convert_expr(callee)),
            args: args.iter().map(convert_expr).collect(),
        },
        TirExprKind::ClosureToCanonical {
            functor,
            functor_id,
            target_fn_type,
            closure_module,
        } => NirExprKind::ClosureToCanonical {
            functor: Box::new(convert_expr(functor)),
            functor_id: *functor_id,
            target_fn_type: *target_fn_type,
            closure_module: closure_module.clone(),
        },
        TirExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload,
        } => NirExprKind::VariantConstruct {
            variant_type: *variant_type,
            case_index: *case_index,
            case_name: case_name.clone(),
            payload: payload.as_ref().map(|p| Box::new(convert_expr(p))),
        },
        TirExprKind::EnumConstruct {
            enum_type,
            case_index,
            case_name,
        } => NirExprKind::EnumConstruct {
            enum_type: *enum_type,
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        TirExprKind::LabeledBlock {
            label,
            block,
            result_type,
        } => NirExprKind::LabeledBlock {
            label: label.clone(),
            block: convert_block(block),
            result_type: *result_type,
        },
        TirExprKind::VariantTag { expr } => NirExprKind::VariantTag {
            expr: Box::new(convert_expr(expr)),
        },
        TirExprKind::VariantTest {
            expr,
            case_index,
            case_name,
        } => NirExprKind::VariantTest {
            expr: Box::new(convert_expr(expr)),
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        TirExprKind::VariantPayload {
            expr,
            case_index,
            payload_type,
        } => NirExprKind::VariantPayload {
            expr: Box::new(convert_expr(expr)),
            case_index: *case_index,
            payload_type: *payload_type,
        },
        TirExprKind::Switch {
            scrutinee,
            min_value,
            arms,
            default,
        } => NirExprKind::Switch {
            scrutinee: Box::new(convert_expr(scrutinee)),
            min_value: *min_value,
            arms: arms.iter().map(convert_block).collect(),
            default: convert_block(default),
        },
        TirExprKind::TemplateString { .. } => unreachable!(
            "TirExprKind::TemplateString should be expanded by synthesis::template before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::WithHandler { .. } => unreachable!(
            "TirExprKind::WithHandler should be desugared by synthesis::effect_dispatch before nir_convert::flat_to_nir runs"
        ),
        TirExprKind::Resume { .. } => unreachable!(
            "TirExprKind::Resume should be desugared by synthesis::effect_dispatch before nir_convert::flat_to_nir runs"
        ),
    }
}

fn convert_pattern(pattern: &TirPattern) -> NirPattern {
    match pattern {
        TirPattern::Wildcard => NirPattern::Wildcard,
        TirPattern::Binding {
            name,
            local_index,
            type_id,
        } => NirPattern::Binding {
            name: name.clone(),
            local_index: *local_index,
            type_id: *type_id,
        },
        TirPattern::Literal(lit) => NirPattern::Literal(convert_literal_pattern(lit)),
        TirPattern::Tuple(patterns, has_rest) => {
            NirPattern::Tuple(patterns.iter().map(convert_pattern).collect(), *has_rest)
        }
        TirPattern::Variant {
            enum_type,
            variant_name,
            bindings,
            payload_type,
        } => NirPattern::Variant {
            enum_type: *enum_type,
            variant_name: variant_name.clone(),
            bindings: bindings.iter().map(convert_pattern).collect(),
            payload_type: *payload_type,
        },
        TirPattern::Enum {
            enum_type,
            case_name,
            case_index,
        } => NirPattern::Enum {
            enum_type: *enum_type,
            case_name: case_name.clone(),
            case_index: *case_index,
        },
        TirPattern::Struct {
            struct_type,
            fields,
            has_rest,
        } => NirPattern::Struct {
            struct_type: *struct_type,
            fields: fields.iter().map(convert_struct_pattern_field).collect(),
            has_rest: *has_rest,
        },
        TirPattern::Or(patterns) => NirPattern::Or(patterns.iter().map(convert_pattern).collect()),
        TirPattern::ConstantValue { expr } => NirPattern::ConstantValue {
            expr: Box::new(convert_expr(expr)),
        },
        TirPattern::Range {
            start,
            end,
            inclusive,
            is_unsigned,
        } => NirPattern::Range {
            start: *start,
            end: *end,
            inclusive: *inclusive,
            is_unsigned: *is_unsigned,
        },
    }
}

fn convert_struct_pattern_field(field: &TirStructPatternField) -> NirStructPatternField {
    NirStructPatternField {
        field_name: field.field_name.clone(),
        field_index: field.field_index,
        pattern: convert_pattern(&field.pattern),
    }
}

fn convert_literal_pattern(lit: &TirLiteralPattern) -> NirLiteralPattern {
    match lit {
        TirLiteralPattern::I128(v) => NirLiteralPattern::I128(*v),
        TirLiteralPattern::U128(v) => NirLiteralPattern::U128(*v),
        TirLiteralPattern::Bool(b) => NirLiteralPattern::Bool(*b),
        TirLiteralPattern::Char(c) => NirLiteralPattern::Char(*c),
        TirLiteralPattern::String(s) => NirLiteralPattern::String(s.clone()),
        TirLiteralPattern::Null => NirLiteralPattern::Null,
    }
}

fn convert_match_arm(arm: &TirMatchArm) -> NirMatchArm {
    NirMatchArm {
        pattern: convert_pattern(&arm.pattern),
        guard: arm.guard.as_ref().map(convert_expr),
        body: convert_expr(&arm.body),
        span: arm.span,
    }
}

fn convert_struct_field(field: &TirStructField) -> NirStructField {
    NirStructField {
        name: field.name.clone(),
        value: convert_expr(&field.value),
        field_index: field.field_index,
    }
}

fn convert_capture(c: &TirCapture) -> NirCapture {
    NirCapture {
        name: c.name.clone(),
        outer_index: c.outer_index,
        type_id: c.type_id,
        is_mut: c.is_mut,
    }
}

fn convert_binary_op(op: tir::TirBinaryOp) -> nir::NirBinaryOp {
    match op {
        tir::TirBinaryOp::Add => nir::NirBinaryOp::Add,
        tir::TirBinaryOp::Sub => nir::NirBinaryOp::Sub,
        tir::TirBinaryOp::Mul => nir::NirBinaryOp::Mul,
        tir::TirBinaryOp::Div => nir::NirBinaryOp::Div,
        tir::TirBinaryOp::Mod => nir::NirBinaryOp::Mod,
        tir::TirBinaryOp::Eq => nir::NirBinaryOp::Eq,
        tir::TirBinaryOp::NotEq => nir::NirBinaryOp::NotEq,
        tir::TirBinaryOp::Lt => nir::NirBinaryOp::Lt,
        tir::TirBinaryOp::LtEq => nir::NirBinaryOp::LtEq,
        tir::TirBinaryOp::Gt => nir::NirBinaryOp::Gt,
        tir::TirBinaryOp::GtEq => nir::NirBinaryOp::GtEq,
        tir::TirBinaryOp::And => nir::NirBinaryOp::And,
        tir::TirBinaryOp::Or => nir::NirBinaryOp::Or,
        tir::TirBinaryOp::BitAnd => nir::NirBinaryOp::BitAnd,
        tir::TirBinaryOp::BitOr => nir::NirBinaryOp::BitOr,
        tir::TirBinaryOp::BitXor => nir::NirBinaryOp::BitXor,
        tir::TirBinaryOp::Shl => nir::NirBinaryOp::Shl,
        tir::TirBinaryOp::Shr => nir::NirBinaryOp::Shr,
        tir::TirBinaryOp::RefEq => nir::NirBinaryOp::RefEq,
        tir::TirBinaryOp::RefNotEq => nir::NirBinaryOp::RefNotEq,
    }
}

fn convert_unary_op(op: tir::TirUnaryOp) -> nir::NirUnaryOp {
    match op {
        tir::TirUnaryOp::Neg => nir::NirUnaryOp::Neg,
        tir::TirUnaryOp::Not => nir::NirUnaryOp::Not,
        tir::TirUnaryOp::BitNot => nir::NirUnaryOp::BitNot,
        tir::TirUnaryOp::Ref => nir::NirUnaryOp::Ref,
        tir::TirUnaryOp::MutRef => nir::NirUnaryOp::MutRef,
        tir::TirUnaryOp::Deref => nir::NirUnaryOp::Deref,
    }
}

fn convert_local(local: &TirLocal) -> NirLocal {
    NirLocal {
        name: local.name.clone(),
        type_id: local.type_id,
        is_mut: local.is_mut,
    }
}

fn convert_param(param: &TirParam) -> NirParam {
    NirParam {
        name: param.name.clone(),
        type_id: param.type_id,
        local_index: param.local_index,
        is_mut: param.is_mut,
        default_expr: param
            .default_expr
            .as_ref()
            .map(|e| Box::new(convert_expr(e))),
        span: param.span,
    }
}

fn convert_type_param(tp: &TirTypeParam) -> NirTypeParam {
    NirTypeParam {
        name: tp.name.clone(),
        is_effect: tp.is_effect,
        is_pack: tp.is_pack,
        bounds: tp.bounds.clone(),
        default: tp.default,
        index: tp.index,
    }
}

fn convert_monomorph_info(info: &MonomorphInfo) -> nir::MonomorphInfo {
    nir::MonomorphInfo {
        generic_name: info.generic_name.clone(),
        impl_type_args: info.impl_type_args.clone(),
        method_type_args: info.method_type_args.clone(),
        is_blanket: info.is_blanket,
    }
}

fn convert_function_ref(func: &FunctionRef) -> nir::FunctionRef {
    nir::FunctionRef {
        module_source: func.module_source.clone(),
        name: func.name.clone(),
        monomorph_info: func.monomorph_info.as_ref().map(convert_monomorph_info),
        method_info: func.method_info.clone(),
    }
}

fn convert_call_arg(arg: &CallArg) -> nir::CallArg {
    nir::CallArg {
        expr: convert_expr(&arg.expr),
        is_mut: arg.is_mut,
    }
}

fn convert_function_kind(kind: &tir::FunctionKind) -> nir::FunctionKind {
    match kind {
        tir::FunctionKind::Regular => nir::FunctionKind::Regular,
        tir::FunctionKind::ValueCopy { type_id } => {
            nir::FunctionKind::ValueCopy { type_id: *type_id }
        }
        tir::FunctionKind::FnCanonicalDispatch {
            trait_kind,
            arity,
            return_type,
        } => nir::FunctionKind::FnCanonicalDispatch {
            trait_kind: convert_fn_dispatch_trait(*trait_kind),
            arity: *arity,
            return_type: *return_type,
        },
    }
}

fn convert_inline_hint(hint: tir::InlineHint) -> nir::InlineHint {
    match hint {
        tir::InlineHint::Auto => nir::InlineHint::Auto,
        tir::InlineHint::Hint => nir::InlineHint::Hint,
        tir::InlineHint::Always => nir::InlineHint::Always,
        tir::InlineHint::Never => nir::InlineHint::Never,
    }
}

fn convert_return_abi(abi: &tir::ReturnAbi) -> nir::ReturnAbi {
    match abi {
        tir::ReturnAbi::Single => nir::ReturnAbi::Single,
        tir::ReturnAbi::MultiValue {
            result_types,
            field_names,
        } => nir::ReturnAbi::MultiValue {
            result_types: result_types.clone(),
            field_names: field_names.clone(),
        },
    }
}

fn convert_fn_dispatch_trait(kind: tir::FnDispatchTrait) -> nir::FnDispatchTrait {
    match kind {
        tir::FnDispatchTrait::Inspect => nir::FnDispatchTrait::Inspect,
        tir::FnDispatchTrait::InspectAlt => nir::FnDispatchTrait::InspectAlt,
    }
}

fn convert_field(field: &TirField) -> NirField {
    NirField {
        name: field.name.clone(),
        is_pub: field.is_pub,
        type_id: field.type_id,
        index: field.index,
        span: field.span,
        is_hidden: field.is_hidden,
        serde_rename: field.serde_rename.clone(),
        serde_default: field.serde_default,
        default_expr: field
            .default_expr
            .as_ref()
            .map(|e| Box::new(convert_expr(e))),
    }
}

fn convert_enum_case(case: &TirEnumCase) -> NirEnumCase {
    NirEnumCase {
        name: case.name.clone(),
        index: case.index,
        span: case.span,
    }
}

fn convert_flags_member(m: &TirFlagsMember) -> NirFlagsMember {
    NirFlagsMember {
        name: m.name.clone(),
        bitmask: m.bitmask,
        span: m.span,
    }
}

fn convert_variant_case(case: &TirVariantCase) -> NirVariantCase {
    NirVariantCase {
        name: case.name.clone(),
        index: case.index,
        payload: case.payload,
        span: case.span,
    }
}
