//! `FlatPackage` → `NirPackage` conversion.
//!
//! Field-for-field reconstruction: TIR and NIR body types have identical
//! shapes (NIR is a renamed copy of TIR). See `docs/wep-2026-05-11-nir.md`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::nir;
use crate::nir::{
    NirBlock, NirCapture, NirEnum, NirEnumCase, NirExpr, NirExprKind, NirField, NirFlags,
    NirFlagsMember, NirFunction, NirGlobal, NirHandlerBinding, NirImport, NirLiteralPattern,
    NirLocal, NirMatchArm, NirParam, NirPattern, NirStmt, NirStmtKind, NirStruct, NirStructField,
    NirStructPatternField, NirTemplatePart, NirTest, NirTypeParam, NirVariantCase, NirVariantDecl,
};
use crate::nir_package::NirPackage;
use crate::tir;
use crate::tir::{
    CallArg, ClosureFunctor, FunctionRef, MonomorphInfo, TirBlock, TirCapture, TirEnum,
    TirEnumCase, TirExpr, TirExprKind, TirField, TirFlags, TirFlagsMember, TirFunction, TirGlobal,
    TirHandlerBinding, TirImport, TirLiteralPattern, TirLocal, TirMatchArm, TirParam, TirPattern,
    TirStmt, TirStmtKind, TirStruct, TirStructField, TirStructPatternField, TirTemplatePart,
    TirTest, TirTypeParam, TirVariantCase, TirVariantDecl,
};

/// Convert a [`FlatPackage`] (TIR-shaped) into a [`NirPackage`] (NIR-shaped).
///
/// TIR and NIR body types are structurally identical; this is a field-for-field
/// reconstruction that recurses into nested expressions, statements, and
/// patterns. Shared type-system data (`TypeTable`, registries, plans) is
/// `.clone()`-d directly.
pub fn flat_to_nir(flat: &FlatPackage) -> NirPackage {
    // Convert each function once and remember the source `Rc`'s address so
    // `ClosureFunctor::call_method` can reuse the same fresh `Rc` instead of
    // allocating a sibling — the optimizer's closure-type DCE pass keys on
    // `Rc::ptr_eq` between `functor.call_method` and `project.functions[i]`.
    let mut func_map: std::collections::HashMap<*const RefCell<TirFunction>, Rc<RefCell<NirFunction>>> =
        std::collections::HashMap::with_capacity(flat.functions.len());
    let functions: Vec<Rc<RefCell<NirFunction>>> = flat
        .functions
        .iter()
        .map(|func_rc| {
            let ptr = Rc::as_ptr(func_rc);
            let nir_rc = Rc::new(RefCell::new(convert_function(&func_rc.borrow())));
            func_map.insert(ptr, Rc::clone(&nir_rc));
            nir_rc
        })
        .collect();
    NirPackage {
        entry_module_source: flat.entry_module_source.clone(),
        type_table: Rc::clone(&flat.type_table),
        functions,
        structs: flat.structs.iter().map(convert_struct).collect(),
        enums: flat.enums.iter().map(convert_enum).collect(),
        variants: flat.variants.iter().map(convert_variant_decl).collect(),
        variant_index: flat.variant_index.clone(),
        flags: flat.flags.iter().map(convert_flags).collect(),
        globals: flat.globals.iter().map(convert_global).collect(),
        imports: flat.imports.iter().map(convert_import).collect(),
        tests: flat.tests.iter().map(convert_test).collect(),
        string_literals: flat.string_literals.clone(),
        bytes_literals: flat.bytes_literals.clone(),
        closure_functors: flat
            .closure_functors
            .iter()
            .map(|cf| convert_closure_functor(cf, &func_map))
            .collect(),
        function_strings: flat.function_strings.clone(),
        function_method_info: flat.function_method_info.clone(),
        wasm_module_sources: flat.wasm_module_sources.clone(),
        module_name: flat.module_name.clone(),
        wasi_registry: flat.wasi_registry,
        world_registry: flat.world_registry,
        used_wasi_functions: flat.used_wasi_functions.clone(),
        strip_names: flat.strip_names,
        skip_validation: flat.skip_validation,
        target_world: flat.target_world.clone(),
        has_http_handler_export: flat.has_http_handler_export,
        export_binding_names: flat.export_binding_names.clone(),
        component_plan: flat.component_plan.clone(),
        builtin_registry: flat.builtin_registry.clone(),
        task_return_flat_params: flat.task_return_flat_params.clone(),
        wasm_assets: flat.wasm_assets.clone(),
        trait_env: std::sync::Arc::clone(&flat.trait_env),
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
    func_map: &std::collections::HashMap<*const RefCell<TirFunction>, Rc<RefCell<NirFunction>>>,
) -> nir::ClosureFunctor {
    // Reuse the converted function `Rc` when the functor's `call_method`
    // shares its allocation with one of the package's top-level functions
    // (the common case). Fall back to a fresh conversion only if the
    // functor's call_method is not present in the package's function list.
    let call_method = func_map
        .get(&Rc::as_ptr(&cf.call_method))
        .cloned()
        .unwrap_or_else(|| {
            Rc::new(RefCell::new(convert_function(&cf.call_method.borrow())))
        });
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
        TirStmtKind::TaskReturn { value } => NirStmtKind::TaskReturn {
            value: convert_expr(value),
        },
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
        TirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => NirStmtKind::IfLet {
            scrutinee: convert_expr(scrutinee),
            pattern: convert_pattern(pattern),
            then_block: convert_block(then_block),
            else_block: else_block.as_ref().map(convert_block),
        },
        TirStmtKind::LetDestructure {
            pattern,
            is_mut,
            value,
        } => NirStmtKind::LetDestructure {
            pattern: convert_pattern(pattern),
            is_mut: *is_mut,
            value: convert_expr(value),
        },
        TirStmtKind::VariadicForOf {
            iterable,
            binding_name,
            binding_local,
            is_mut,
            body,
            unique_id,
        } => NirStmtKind::VariadicForOf {
            iterable: convert_expr(iterable),
            binding_name: binding_name.clone(),
            binding_local: *binding_local,
            is_mut: *is_mut,
            body: convert_block(body),
            unique_id: *unique_id,
        },
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
        TirExprKind::FuncRef {
            module_source,
            name,
        } => NirExprKind::FuncRef {
            module_source: module_source.clone(),
            name: name.clone(),
        },
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
        TirExprKind::TupleSpread { expr } => NirExprKind::TupleSpread {
            expr: Box::new(convert_expr(expr)),
        },
        TirExprKind::TupleZip { expr } => NirExprKind::TupleZip {
            expr: Box::new(convert_expr(expr)),
        },
        TirExprKind::TypePackExpansion {
            call_expr,
            pack_type_id,
        } => NirExprKind::TypePackExpansion {
            call_expr: Box::new(convert_expr(call_expr)),
            pack_type_id: *pack_type_id,
        },
        TirExprKind::Capture { index, name } => NirExprKind::Capture {
            index: *index,
            name: name.clone(),
        },
        TirExprKind::Closure {
            params,
            body,
            captures,
            functor_id,
            address_taken_locals,
            body_locals,
        } => NirExprKind::Closure {
            params: params.clone(),
            body: Box::new(convert_expr(body)),
            captures: captures.iter().map(convert_capture).collect(),
            functor_id: *functor_id,
            address_taken_locals: address_taken_locals.clone(),
            body_locals: body_locals.iter().map(convert_local).collect(),
        },
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
        TirExprKind::TemplateString { parts } => NirExprKind::TemplateString {
            parts: parts.iter().map(convert_template_part).collect(),
        },
        TirExprKind::WithHandler {
            bindings,
            body,
            result_type,
        } => NirExprKind::WithHandler {
            bindings: bindings.iter().map(convert_handler_binding).collect(),
            body: convert_block(body),
            result_type: *result_type,
        },
        TirExprKind::Resume { value } => NirExprKind::Resume {
            value: Box::new(convert_expr(value)),
        },
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
        TirPattern::Tuple(patterns, has_rest) => NirPattern::Tuple(
            patterns.iter().map(convert_pattern).collect(),
            *has_rest,
        ),
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
        TirPattern::Or(patterns) => {
            NirPattern::Or(patterns.iter().map(convert_pattern).collect())
        }
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

fn convert_handler_binding(binding: &TirHandlerBinding) -> NirHandlerBinding {
    NirHandlerBinding {
        effect: binding.effect.clone(),
        trait_type_args: binding.trait_type_args.clone(),
        handler: convert_expr(&binding.handler),
        handler_type: binding.handler_type,
        span: binding.span,
        bundle_group: binding.bundle_group,
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

fn convert_template_part(part: &TirTemplatePart) -> NirTemplatePart {
    match part {
        TirTemplatePart::Literal(s) => NirTemplatePart::Literal(s.clone()),
        TirTemplatePart::Interpolation { expr, format_spec } => NirTemplatePart::Interpolation {
            expr: Box::new(convert_expr(expr)),
            format_spec: format_spec.as_ref().map(convert_template_format_spec),
        },
    }
}

fn convert_template_format_spec(spec: &tir::TemplateFormatSpec) -> nir::TemplateFormatSpec {
    nir::TemplateFormatSpec {
        fill: spec.fill,
        align: spec.align,
        sign_plus: spec.sign_plus,
        alternate: spec.alternate,
        zero_pad: spec.zero_pad,
        width: spec.width,
        precision: spec.precision,
        type_char: spec.type_char,
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
        tir::FunctionKind::ValueCopy { type_id } => nir::FunctionKind::ValueCopy {
            type_id: *type_id,
        },
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

// ---------------------------------------------------------------------------
// Temporary `NirPackage` → `FlatPackage` inverse adapter (WEP Phase 2).
//
// This entire section will be deleted in WEP Phase 5 once downstream
// consumers natively consume `NirPackage`. All inverse helpers share the
// `_back` suffix so they can be grep-deleted as a unit.
// ---------------------------------------------------------------------------

/// Temporary inverse of [`flat_to_nir`]. Will be removed in WEP Phase 5
/// once downstream consumers natively consume `NirPackage`.
#[allow(dead_code)]
pub fn nir_to_flat(nir: &NirPackage) -> FlatPackage {
    // Same identity preservation as `flat_to_nir` — closure functors must
    // reuse the same Rc as the corresponding package function.
    let mut func_map: std::collections::HashMap<*const RefCell<NirFunction>, Rc<RefCell<TirFunction>>> =
        std::collections::HashMap::with_capacity(nir.functions.len());
    let functions: Vec<Rc<RefCell<TirFunction>>> = nir
        .functions
        .iter()
        .map(|func_rc| {
            let ptr = Rc::as_ptr(func_rc);
            let tir_rc = Rc::new(RefCell::new(convert_function_back(&func_rc.borrow())));
            func_map.insert(ptr, Rc::clone(&tir_rc));
            tir_rc
        })
        .collect();
    FlatPackage {
        entry_module_source: nir.entry_module_source.clone(),
        type_table: Rc::clone(&nir.type_table),
        functions,
        structs: nir.structs.iter().map(convert_struct_back).collect(),
        enums: nir.enums.iter().map(convert_enum_back).collect(),
        variants: nir.variants.iter().map(convert_variant_decl_back).collect(),
        variant_index: nir.variant_index.clone(),
        flags: nir.flags.iter().map(convert_flags_back).collect(),
        globals: nir.globals.iter().map(convert_global_back).collect(),
        imports: nir.imports.iter().map(convert_import_back).collect(),
        tests: nir.tests.iter().map(convert_test_back).collect(),
        string_literals: nir.string_literals.clone(),
        bytes_literals: nir.bytes_literals.clone(),
        closure_functors: nir
            .closure_functors
            .iter()
            .map(|cf| convert_closure_functor_back(cf, &func_map))
            .collect(),
        function_strings: nir.function_strings.clone(),
        function_method_info: nir.function_method_info.clone(),
        wasm_module_sources: nir.wasm_module_sources.clone(),
        module_name: nir.module_name.clone(),
        wasi_registry: nir.wasi_registry,
        world_registry: nir.world_registry,
        used_wasi_functions: nir.used_wasi_functions.clone(),
        strip_names: nir.strip_names,
        skip_validation: nir.skip_validation,
        target_world: nir.target_world.clone(),
        has_http_handler_export: nir.has_http_handler_export,
        export_binding_names: nir.export_binding_names.clone(),
        component_plan: nir.component_plan.clone(),
        builtin_registry: nir.builtin_registry.clone(),
        task_return_flat_params: nir.task_return_flat_params.clone(),
        wasm_assets: nir.wasm_assets.clone(),
        trait_env: std::sync::Arc::clone(&nir.trait_env),
    }
}

#[allow(dead_code)]
fn convert_function_back(func: &NirFunction) -> TirFunction {
    TirFunction {
        name: func.name.clone(),
        module_source: func.module_source.clone(),
        is_pub: func.is_pub,
        is_export: func.is_export,
        is_async: func.is_async,
        type_params: func
            .type_params
            .iter()
            .map(convert_type_param_back)
            .collect(),
        impl_type_params: func
            .impl_type_params
            .iter()
            .map(convert_type_param_back)
            .collect(),
        monomorph_info: func
            .monomorph_info
            .as_ref()
            .map(convert_monomorph_info_back),
        method_info: func.method_info.clone(),
        params: func.params.iter().map(convert_param_back).collect(),
        return_type: func.return_type,
        task_return_type: func.task_return_type,
        effects: func.effects.clone(),
        stores: func.stores.clone(),
        body: func.body.as_ref().map(convert_block_back),
        span: func.span,
        local_count: func.local_count,
        locals: func.locals.iter().map(convert_local_back).collect(),
        address_taken_locals: func.address_taken_locals.clone(),
        stores_aliased_locals: func.stores_aliased_locals.clone(),
        is_cm_binding: func.is_cm_binding,
        is_dispatch_wrapper: func.is_dispatch_wrapper,
        is_cm_export: func.is_cm_export,
        is_ambient: func.is_ambient,
        inline_hint: convert_inline_hint_back(func.inline_hint),
        comp_features: func.comp_features,
        export_name: func.export_name.clone(),
        allocator_tag: func.allocator_tag.clone(),
        kind: convert_function_kind_back(&func.kind),
        return_abi: convert_return_abi_back(&func.return_abi),
    }
}

#[allow(dead_code)]
fn convert_global_back(global: &NirGlobal) -> TirGlobal {
    TirGlobal {
        name: global.name.clone(),
        ty: global.ty,
        initializer: convert_expr_back(&global.initializer),
        mutable: global.mutable,
        wado_mutable: global.wado_mutable,
        is_pub: global.is_pub,
        module_source: global.module_source.clone(),
        span: global.span,
        is_nullable: global.is_nullable,
        lazy_init: global.lazy_init,
        locals: global.locals.iter().map(convert_local_back).collect(),
    }
}

#[allow(dead_code)]
fn convert_test_back(test: &NirTest) -> TirTest {
    TirTest {
        name: test.name.clone(),
        function_name: test.function_name.clone(),
        line: test.line,
        span: test.span,
        expect_trap: test.expect_trap,
        is_todo: test.is_todo,
        timeout_ms: test.timeout_ms,
    }
}

#[allow(dead_code)]
fn convert_struct_back(s: &NirStruct) -> TirStruct {
    TirStruct {
        name: s.name.clone(),
        module_source: s.module_source.clone(),
        is_pub: s.is_pub,
        type_params: s.type_params.iter().map(convert_type_param_back).collect(),
        monomorph_info: s.monomorph_info.as_ref().map(convert_monomorph_info_back),
        fields: s.fields.iter().map(convert_field_back).collect(),
        span: s.span,
        serde_rename_all: s.serde_rename_all.clone(),
    }
}

#[allow(dead_code)]
fn convert_enum_back(e: &NirEnum) -> TirEnum {
    TirEnum {
        name: e.name.clone(),
        module_source: e.module_source.clone(),
        is_pub: e.is_pub,
        type_params: e.type_params.iter().map(convert_type_param_back).collect(),
        monomorph_info: e.monomorph_info.as_ref().map(convert_monomorph_info_back),
        cases: e.cases.iter().map(convert_enum_case_back).collect(),
        span: e.span,
    }
}

#[allow(dead_code)]
fn convert_flags_back(f: &NirFlags) -> TirFlags {
    TirFlags {
        name: f.name.clone(),
        module_source: f.module_source.clone(),
        is_pub: f.is_pub,
        type_id: f.type_id,
        members: f.members.iter().map(convert_flags_member_back).collect(),
        span: f.span,
    }
}

#[allow(dead_code)]
fn convert_variant_decl_back(v: &NirVariantDecl) -> TirVariantDecl {
    TirVariantDecl {
        name: v.name.clone(),
        module_source: v.module_source.clone(),
        is_pub: v.is_pub,
        type_params: v.type_params.iter().map(convert_type_param_back).collect(),
        cases: v.cases.iter().map(convert_variant_case_back).collect(),
        comp_features: v.comp_features,
        span: v.span,
    }
}

#[allow(dead_code)]
fn convert_import_back(i: &NirImport) -> TirImport {
    TirImport {
        namespace: i.namespace.clone(),
        canonical_name: i.canonical_name.clone(),
        func_name: i.func_name.clone(),
        params: i.params.clone(),
        return_type: i.return_type,
    }
}

#[allow(dead_code)]
fn convert_closure_functor_back(
    cf: &nir::ClosureFunctor,
    func_map: &std::collections::HashMap<*const RefCell<NirFunction>, Rc<RefCell<TirFunction>>>,
) -> ClosureFunctor {
    let call_method = func_map
        .get(&Rc::as_ptr(&cf.call_method))
        .cloned()
        .unwrap_or_else(|| {
            Rc::new(RefCell::new(convert_function_back(&cf.call_method.borrow())))
        });
    ClosureFunctor {
        module_source: cf.module_source.clone(),
        id: cf.id,
        struct_name: cf.struct_name.clone(),
        struct_type_id: cf.struct_type_id,
        ref_type_id: cf.ref_type_id,
        call_method,
        captures: cf.captures.iter().map(convert_capture_back).collect(),
        canonical_user_params: cf.canonical_user_params.clone(),
        canonical_return: cf.canonical_return,
    }
}

#[allow(dead_code)]
fn convert_block_back(block: &NirBlock) -> TirBlock {
    TirBlock {
        stmts: block.stmts.iter().map(convert_stmt_back).collect(),
        span: block.span,
    }
}

#[allow(dead_code)]
fn convert_stmt_back(stmt: &NirStmt) -> TirStmt {
    TirStmt {
        kind: convert_stmt_kind_back(&stmt.kind),
        span: stmt.span,
    }
}

#[allow(dead_code)]
fn convert_stmt_kind_back(kind: &NirStmtKind) -> TirStmtKind {
    match kind {
        NirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
            skip_value_copy,
        } => TirStmtKind::Let {
            name: name.clone(),
            local_index: *local_index,
            is_mut: *is_mut,
            is_reactive: *is_reactive,
            type_id: *type_id,
            value: convert_expr_back(value),
            skip_value_copy: *skip_value_copy,
        },
        NirStmtKind::Expr(expr) => TirStmtKind::Expr(convert_expr_back(expr)),
        NirStmtKind::Return { value } => TirStmtKind::Return {
            value: value.as_ref().map(convert_expr_back),
        },
        NirStmtKind::TaskReturn { value } => TirStmtKind::TaskReturn {
            value: convert_expr_back(value),
        },
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => TirStmtKind::If {
            condition: convert_expr_back(condition),
            then_block: convert_block_back(then_block),
            else_block: else_block.as_ref().map(convert_block_back),
        },
        NirStmtKind::Loop { body } => TirStmtKind::Loop {
            body: convert_block_back(body),
        },
        NirStmtKind::Break { label, value } => TirStmtKind::Break {
            label: label.clone(),
            value: value.as_ref().map(convert_expr_back),
        },
        NirStmtKind::Continue => TirStmtKind::Continue,
        NirStmtKind::LabeledBlock { label, block } => TirStmtKind::LabeledBlock {
            label: label.clone(),
            block: convert_block_back(block),
        },
        NirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => TirStmtKind::IfLet {
            scrutinee: convert_expr_back(scrutinee),
            pattern: convert_pattern_back(pattern),
            then_block: convert_block_back(then_block),
            else_block: else_block.as_ref().map(convert_block_back),
        },
        NirStmtKind::LetDestructure {
            pattern,
            is_mut,
            value,
        } => TirStmtKind::LetDestructure {
            pattern: convert_pattern_back(pattern),
            is_mut: *is_mut,
            value: convert_expr_back(value),
        },
        NirStmtKind::VariadicForOf {
            iterable,
            binding_name,
            binding_local,
            is_mut,
            body,
            unique_id,
        } => TirStmtKind::VariadicForOf {
            iterable: convert_expr_back(iterable),
            binding_name: binding_name.clone(),
            binding_local: *binding_local,
            is_mut: *is_mut,
            body: convert_block_back(body),
            unique_id: *unique_id,
        },
    }
}

#[allow(dead_code)]
fn convert_expr_back(expr: &NirExpr) -> TirExpr {
    TirExpr {
        kind: convert_expr_kind_back(&expr.kind),
        type_id: expr.type_id,
        span: expr.span,
    }
}

#[allow(dead_code)]
fn convert_expr_kind_back(kind: &NirExprKind) -> TirExprKind {
    match kind {
        NirExprKind::IntLiteral { value, repr } => TirExprKind::IntLiteral {
            value: *value,
            repr: repr.clone(),
        },
        NirExprKind::FloatLiteral { value, repr } => TirExprKind::FloatLiteral {
            value: *value,
            repr: repr.clone(),
        },
        NirExprKind::BoolLiteral(b) => TirExprKind::BoolLiteral(*b),
        NirExprKind::CharLiteral(c) => TirExprKind::CharLiteral(*c),
        NirExprKind::StringLiteral(s) => TirExprKind::StringLiteral(s.clone()),
        NirExprKind::BytesLiteral(b) => TirExprKind::BytesLiteral(b.clone()),
        NirExprKind::Null => TirExprKind::Null,
        NirExprKind::Unit => TirExprKind::Unit,
        NirExprKind::Local { index, name } => TirExprKind::Local {
            index: *index,
            name: name.clone(),
        },
        NirExprKind::FuncRef {
            module_source,
            name,
        } => TirExprKind::FuncRef {
            module_source: module_source.clone(),
            name: name.clone(),
        },
        NirExprKind::GlobalVarGet {
            module_source,
            name,
        } => TirExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.clone(),
        },
        NirExprKind::GlobalVarSet {
            module_source,
            name,
            value,
        } => TirExprKind::GlobalVarSet {
            module_source: module_source.clone(),
            name: name.clone(),
            value: Box::new(convert_expr_back(value)),
        },
        NirExprKind::Binary { left, op, right } => TirExprKind::Binary {
            left: Box::new(convert_expr_back(left)),
            op: convert_binary_op_back(*op),
            right: Box::new(convert_expr_back(right)),
        },
        NirExprKind::Unary { op, expr } => TirExprKind::Unary {
            op: convert_unary_op_back(*op),
            expr: Box::new(convert_expr_back(expr)),
        },
        NirExprKind::Assign { target, value } => TirExprKind::Assign {
            target: Box::new(convert_expr_back(target)),
            value: Box::new(convert_expr_back(value)),
        },
        NirExprKind::Cast { expr, target_type } => TirExprKind::Cast {
            expr: Box::new(convert_expr_back(expr)),
            target_type: *target_type,
        },
        NirExprKind::Call {
            func,
            type_args,
            args,
        } => TirExprKind::Call {
            func: convert_function_ref_back(func),
            type_args: type_args.clone(),
            args: args.iter().map(convert_call_arg_back).collect(),
        },
        NirExprKind::CmRawCall { local_name, args } => TirExprKind::CmRawCall {
            local_name: local_name.clone(),
            args: args.iter().map(convert_expr_back).collect(),
        },
        NirExprKind::MethodCall {
            receiver,
            func,
            type_args,
            args,
            ..
        } => crate::tir::TirExprKind::method_call(
            Box::new(convert_expr_back(receiver)),
            convert_function_ref_back(func),
            type_args.clone(),
            args.iter().map(convert_call_arg_back).collect(),
        ),
        NirExprKind::FieldAccess {
            expr,
            field_index,
            field_name,
        } => TirExprKind::FieldAccess {
            expr: Box::new(convert_expr_back(expr)),
            field_index: *field_index,
            field_name: field_name.clone(),
        },
        NirExprKind::Index { expr, index } => TirExprKind::Index {
            expr: Box::new(convert_expr_back(expr)),
            index: Box::new(convert_expr_back(index)),
        },
        NirExprKind::Block(block) => TirExprKind::Block(convert_block_back(block)),
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => TirExprKind::If {
            condition: Box::new(convert_expr_back(condition)),
            then_branch: convert_block_back(then_branch),
            else_branch: else_branch.as_ref().map(convert_block_back),
        },
        NirExprKind::Match { expr, arms } => TirExprKind::Match {
            expr: Box::new(convert_expr_back(expr)),
            arms: arms.iter().map(convert_match_arm_back).collect(),
        },
        NirExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } => TirExprKind::StructLiteral {
            struct_type: *struct_type,
            struct_name: struct_name.clone(),
            fields: fields.iter().map(convert_struct_field_back).collect(),
        },
        NirExprKind::TupleLiteral { elements } => TirExprKind::TupleLiteral {
            elements: elements.iter().map(convert_expr_back).collect(),
        },
        NirExprKind::TupleSpread { expr } => TirExprKind::TupleSpread {
            expr: Box::new(convert_expr_back(expr)),
        },
        NirExprKind::TupleZip { expr } => TirExprKind::TupleZip {
            expr: Box::new(convert_expr_back(expr)),
        },
        NirExprKind::TypePackExpansion {
            call_expr,
            pack_type_id,
        } => TirExprKind::TypePackExpansion {
            call_expr: Box::new(convert_expr_back(call_expr)),
            pack_type_id: *pack_type_id,
        },
        NirExprKind::Capture { index, name } => TirExprKind::Capture {
            index: *index,
            name: name.clone(),
        },
        NirExprKind::Closure {
            params,
            body,
            captures,
            functor_id,
            address_taken_locals,
            body_locals,
        } => TirExprKind::Closure {
            params: params.clone(),
            body: Box::new(convert_expr_back(body)),
            captures: captures.iter().map(convert_capture_back).collect(),
            functor_id: *functor_id,
            address_taken_locals: address_taken_locals.clone(),
            body_locals: body_locals.iter().map(convert_local_back).collect(),
        },
        NirExprKind::IndirectCall { callee, args } => TirExprKind::IndirectCall {
            callee: Box::new(convert_expr_back(callee)),
            args: args.iter().map(convert_expr_back).collect(),
        },
        NirExprKind::ClosureToCanonical {
            functor,
            functor_id,
            target_fn_type,
            closure_module,
        } => TirExprKind::ClosureToCanonical {
            functor: Box::new(convert_expr_back(functor)),
            functor_id: *functor_id,
            target_fn_type: *target_fn_type,
            closure_module: closure_module.clone(),
        },
        NirExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload,
        } => TirExprKind::VariantConstruct {
            variant_type: *variant_type,
            case_index: *case_index,
            case_name: case_name.clone(),
            payload: payload.as_ref().map(|p| Box::new(convert_expr_back(p))),
        },
        NirExprKind::EnumConstruct {
            enum_type,
            case_index,
            case_name,
        } => TirExprKind::EnumConstruct {
            enum_type: *enum_type,
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        NirExprKind::LabeledBlock {
            label,
            block,
            result_type,
        } => TirExprKind::LabeledBlock {
            label: label.clone(),
            block: convert_block_back(block),
            result_type: *result_type,
        },
        NirExprKind::VariantTag { expr } => TirExprKind::VariantTag {
            expr: Box::new(convert_expr_back(expr)),
        },
        NirExprKind::VariantTest {
            expr,
            case_index,
            case_name,
        } => TirExprKind::VariantTest {
            expr: Box::new(convert_expr_back(expr)),
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        NirExprKind::VariantPayload {
            expr,
            case_index,
            payload_type,
        } => TirExprKind::VariantPayload {
            expr: Box::new(convert_expr_back(expr)),
            case_index: *case_index,
            payload_type: *payload_type,
        },
        NirExprKind::Switch {
            scrutinee,
            min_value,
            arms,
            default,
        } => TirExprKind::Switch {
            scrutinee: Box::new(convert_expr_back(scrutinee)),
            min_value: *min_value,
            arms: arms.iter().map(convert_block_back).collect(),
            default: convert_block_back(default),
        },
        NirExprKind::TemplateString { parts } => TirExprKind::TemplateString {
            parts: parts.iter().map(convert_template_part_back).collect(),
        },
        NirExprKind::WithHandler {
            bindings,
            body,
            result_type,
        } => TirExprKind::WithHandler {
            bindings: bindings.iter().map(convert_handler_binding_back).collect(),
            body: convert_block_back(body),
            result_type: *result_type,
        },
        NirExprKind::Resume { value } => TirExprKind::Resume {
            value: Box::new(convert_expr_back(value)),
        },
    }
}

#[allow(dead_code)]
fn convert_pattern_back(pattern: &NirPattern) -> TirPattern {
    match pattern {
        NirPattern::Wildcard => TirPattern::Wildcard,
        NirPattern::Binding {
            name,
            local_index,
            type_id,
        } => TirPattern::Binding {
            name: name.clone(),
            local_index: *local_index,
            type_id: *type_id,
        },
        NirPattern::Literal(lit) => TirPattern::Literal(convert_literal_pattern_back(lit)),
        NirPattern::Tuple(patterns, has_rest) => TirPattern::Tuple(
            patterns.iter().map(convert_pattern_back).collect(),
            *has_rest,
        ),
        NirPattern::Variant {
            enum_type,
            variant_name,
            bindings,
            payload_type,
        } => TirPattern::Variant {
            enum_type: *enum_type,
            variant_name: variant_name.clone(),
            bindings: bindings.iter().map(convert_pattern_back).collect(),
            payload_type: *payload_type,
        },
        NirPattern::Enum {
            enum_type,
            case_name,
            case_index,
        } => TirPattern::Enum {
            enum_type: *enum_type,
            case_name: case_name.clone(),
            case_index: *case_index,
        },
        NirPattern::Struct {
            struct_type,
            fields,
            has_rest,
        } => TirPattern::Struct {
            struct_type: *struct_type,
            fields: fields
                .iter()
                .map(convert_struct_pattern_field_back)
                .collect(),
            has_rest: *has_rest,
        },
        NirPattern::Or(patterns) => {
            TirPattern::Or(patterns.iter().map(convert_pattern_back).collect())
        }
        NirPattern::ConstantValue { expr } => TirPattern::ConstantValue {
            expr: Box::new(convert_expr_back(expr)),
        },
        NirPattern::Range {
            start,
            end,
            inclusive,
            is_unsigned,
        } => TirPattern::Range {
            start: *start,
            end: *end,
            inclusive: *inclusive,
            is_unsigned: *is_unsigned,
        },
    }
}

#[allow(dead_code)]
fn convert_struct_pattern_field_back(field: &NirStructPatternField) -> TirStructPatternField {
    TirStructPatternField {
        field_name: field.field_name.clone(),
        field_index: field.field_index,
        pattern: convert_pattern_back(&field.pattern),
    }
}

#[allow(dead_code)]
fn convert_literal_pattern_back(lit: &NirLiteralPattern) -> TirLiteralPattern {
    match lit {
        NirLiteralPattern::I128(v) => TirLiteralPattern::I128(*v),
        NirLiteralPattern::U128(v) => TirLiteralPattern::U128(*v),
        NirLiteralPattern::Bool(b) => TirLiteralPattern::Bool(*b),
        NirLiteralPattern::Char(c) => TirLiteralPattern::Char(*c),
        NirLiteralPattern::String(s) => TirLiteralPattern::String(s.clone()),
        NirLiteralPattern::Null => TirLiteralPattern::Null,
    }
}

#[allow(dead_code)]
fn convert_match_arm_back(arm: &NirMatchArm) -> TirMatchArm {
    TirMatchArm {
        pattern: convert_pattern_back(&arm.pattern),
        guard: arm.guard.as_ref().map(convert_expr_back),
        body: convert_expr_back(&arm.body),
        span: arm.span,
    }
}

#[allow(dead_code)]
fn convert_handler_binding_back(binding: &NirHandlerBinding) -> TirHandlerBinding {
    TirHandlerBinding {
        effect: binding.effect.clone(),
        trait_type_args: binding.trait_type_args.clone(),
        handler: convert_expr_back(&binding.handler),
        handler_type: binding.handler_type,
        span: binding.span,
        bundle_group: binding.bundle_group,
    }
}

#[allow(dead_code)]
fn convert_struct_field_back(field: &NirStructField) -> TirStructField {
    TirStructField {
        name: field.name.clone(),
        value: convert_expr_back(&field.value),
        field_index: field.field_index,
    }
}

#[allow(dead_code)]
fn convert_capture_back(c: &NirCapture) -> TirCapture {
    TirCapture {
        name: c.name.clone(),
        outer_index: c.outer_index,
        type_id: c.type_id,
        is_mut: c.is_mut,
    }
}

#[allow(dead_code)]
fn convert_template_part_back(part: &NirTemplatePart) -> TirTemplatePart {
    match part {
        NirTemplatePart::Literal(s) => TirTemplatePart::Literal(s.clone()),
        NirTemplatePart::Interpolation { expr, format_spec } => TirTemplatePart::Interpolation {
            expr: Box::new(convert_expr_back(expr)),
            format_spec: format_spec.as_ref().map(convert_template_format_spec_back),
        },
    }
}

#[allow(dead_code)]
fn convert_template_format_spec_back(spec: &nir::TemplateFormatSpec) -> tir::TemplateFormatSpec {
    tir::TemplateFormatSpec {
        fill: spec.fill,
        align: spec.align,
        sign_plus: spec.sign_plus,
        alternate: spec.alternate,
        zero_pad: spec.zero_pad,
        width: spec.width,
        precision: spec.precision,
        type_char: spec.type_char,
    }
}

#[allow(dead_code)]
fn convert_binary_op_back(op: nir::NirBinaryOp) -> tir::TirBinaryOp {
    match op {
        nir::NirBinaryOp::Add => tir::TirBinaryOp::Add,
        nir::NirBinaryOp::Sub => tir::TirBinaryOp::Sub,
        nir::NirBinaryOp::Mul => tir::TirBinaryOp::Mul,
        nir::NirBinaryOp::Div => tir::TirBinaryOp::Div,
        nir::NirBinaryOp::Mod => tir::TirBinaryOp::Mod,
        nir::NirBinaryOp::Eq => tir::TirBinaryOp::Eq,
        nir::NirBinaryOp::NotEq => tir::TirBinaryOp::NotEq,
        nir::NirBinaryOp::Lt => tir::TirBinaryOp::Lt,
        nir::NirBinaryOp::LtEq => tir::TirBinaryOp::LtEq,
        nir::NirBinaryOp::Gt => tir::TirBinaryOp::Gt,
        nir::NirBinaryOp::GtEq => tir::TirBinaryOp::GtEq,
        nir::NirBinaryOp::And => tir::TirBinaryOp::And,
        nir::NirBinaryOp::Or => tir::TirBinaryOp::Or,
        nir::NirBinaryOp::BitAnd => tir::TirBinaryOp::BitAnd,
        nir::NirBinaryOp::BitOr => tir::TirBinaryOp::BitOr,
        nir::NirBinaryOp::BitXor => tir::TirBinaryOp::BitXor,
        nir::NirBinaryOp::Shl => tir::TirBinaryOp::Shl,
        nir::NirBinaryOp::Shr => tir::TirBinaryOp::Shr,
        nir::NirBinaryOp::RefEq => tir::TirBinaryOp::RefEq,
        nir::NirBinaryOp::RefNotEq => tir::TirBinaryOp::RefNotEq,
    }
}

#[allow(dead_code)]
fn convert_unary_op_back(op: nir::NirUnaryOp) -> tir::TirUnaryOp {
    match op {
        nir::NirUnaryOp::Neg => tir::TirUnaryOp::Neg,
        nir::NirUnaryOp::Not => tir::TirUnaryOp::Not,
        nir::NirUnaryOp::BitNot => tir::TirUnaryOp::BitNot,
        nir::NirUnaryOp::Ref => tir::TirUnaryOp::Ref,
        nir::NirUnaryOp::MutRef => tir::TirUnaryOp::MutRef,
        nir::NirUnaryOp::Deref => tir::TirUnaryOp::Deref,
    }
}

#[allow(dead_code)]
fn convert_local_back(local: &NirLocal) -> TirLocal {
    TirLocal {
        name: local.name.clone(),
        type_id: local.type_id,
        is_mut: local.is_mut,
    }
}

#[allow(dead_code)]
fn convert_param_back(param: &NirParam) -> TirParam {
    TirParam {
        name: param.name.clone(),
        type_id: param.type_id,
        local_index: param.local_index,
        is_mut: param.is_mut,
        default_expr: param
            .default_expr
            .as_ref()
            .map(|e| Box::new(convert_expr_back(e))),
        span: param.span,
    }
}

#[allow(dead_code)]
fn convert_type_param_back(tp: &NirTypeParam) -> TirTypeParam {
    TirTypeParam {
        name: tp.name.clone(),
        is_effect: tp.is_effect,
        is_pack: tp.is_pack,
        bounds: tp.bounds.clone(),
        default: tp.default,
        index: tp.index,
    }
}

#[allow(dead_code)]
fn convert_monomorph_info_back(info: &nir::MonomorphInfo) -> MonomorphInfo {
    MonomorphInfo {
        generic_name: info.generic_name.clone(),
        impl_type_args: info.impl_type_args.clone(),
        method_type_args: info.method_type_args.clone(),
        is_blanket: info.is_blanket,
    }
}

#[allow(dead_code)]
fn convert_function_ref_back(func: &nir::FunctionRef) -> FunctionRef {
    FunctionRef {
        module_source: func.module_source.clone(),
        name: func.name.clone(),
        monomorph_info: func.monomorph_info.as_ref().map(convert_monomorph_info_back),
        method_info: func.method_info.clone(),
    }
}

#[allow(dead_code)]
fn convert_call_arg_back(arg: &nir::CallArg) -> CallArg {
    CallArg {
        expr: convert_expr_back(&arg.expr),
        is_mut: arg.is_mut,
    }
}

#[allow(dead_code)]
fn convert_function_kind_back(kind: &nir::FunctionKind) -> tir::FunctionKind {
    match kind {
        nir::FunctionKind::Regular => tir::FunctionKind::Regular,
        nir::FunctionKind::ValueCopy { type_id } => tir::FunctionKind::ValueCopy {
            type_id: *type_id,
        },
        nir::FunctionKind::FnCanonicalDispatch {
            trait_kind,
            arity,
            return_type,
        } => tir::FunctionKind::FnCanonicalDispatch {
            trait_kind: convert_fn_dispatch_trait_back(*trait_kind),
            arity: *arity,
            return_type: *return_type,
        },
    }
}

#[allow(dead_code)]
fn convert_inline_hint_back(hint: nir::InlineHint) -> tir::InlineHint {
    match hint {
        nir::InlineHint::Auto => tir::InlineHint::Auto,
        nir::InlineHint::Hint => tir::InlineHint::Hint,
        nir::InlineHint::Always => tir::InlineHint::Always,
        nir::InlineHint::Never => tir::InlineHint::Never,
    }
}

#[allow(dead_code)]
fn convert_return_abi_back(abi: &nir::ReturnAbi) -> tir::ReturnAbi {
    match abi {
        nir::ReturnAbi::Single => tir::ReturnAbi::Single,
        nir::ReturnAbi::MultiValue {
            result_types,
            field_names,
        } => tir::ReturnAbi::MultiValue {
            result_types: result_types.clone(),
            field_names: field_names.clone(),
        },
    }
}

#[allow(dead_code)]
fn convert_fn_dispatch_trait_back(kind: nir::FnDispatchTrait) -> tir::FnDispatchTrait {
    match kind {
        nir::FnDispatchTrait::Inspect => tir::FnDispatchTrait::Inspect,
        nir::FnDispatchTrait::InspectAlt => tir::FnDispatchTrait::InspectAlt,
    }
}

#[allow(dead_code)]
fn convert_field_back(field: &NirField) -> TirField {
    TirField {
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
            .map(|e| Box::new(convert_expr_back(e))),
    }
}

#[allow(dead_code)]
fn convert_enum_case_back(case: &NirEnumCase) -> TirEnumCase {
    TirEnumCase {
        name: case.name.clone(),
        index: case.index,
        span: case.span,
    }
}

#[allow(dead_code)]
fn convert_flags_member_back(m: &NirFlagsMember) -> TirFlagsMember {
    TirFlagsMember {
        name: m.name.clone(),
        bitmask: m.bitmask,
        span: m.span,
    }
}

#[allow(dead_code)]
fn convert_variant_case_back(case: &NirVariantCase) -> TirVariantCase {
    TirVariantCase {
        name: case.name.clone(),
        index: case.index,
        payload: case.payload,
        span: case.span,
    }
}
