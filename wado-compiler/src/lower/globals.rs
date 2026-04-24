use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::flat_package::FlatPackage;
use crate::name::ModuleSource;
use crate::tir::FunctionRef;
use crate::tir::{
    FunctionKind, InlineHint, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirGlobal,
    TirModule, TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::wide_int::{create_i128_literal, create_u128_literal};

/// Check if an expression is a constant initializer (can be evaluated at Wasm instantiation time)
fn is_constant_initializer(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit => true,
        TirExprKind::Cast { expr: inner, .. } => is_constant_initializer(inner),
        TirExprKind::Unary { op, expr: inner } => {
            // Negation of literals is constant
            matches!(op, TirUnaryOp::Neg) && is_constant_initializer(inner)
        }
        _ => false,
    }
}

/// Create a default value expression for a type (used for lazy-initialized globals)
fn default_value_for_type(type_id: TypeId, type_table: &TypeTable, span: Span) -> TirExpr {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => match prim {
            crate::tir::PrimitiveType::I8
            | crate::tir::PrimitiveType::I16
            | crate::tir::PrimitiveType::I32
            | crate::tir::PrimitiveType::U8
            | crate::tir::PrimitiveType::U16
            | crate::tir::PrimitiveType::U32 => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::I64 | crate::tir::PrimitiveType::U64 => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::I128 | crate::tir::PrimitiveType::U128 => {
                // i128/u128 need special handling - call from_i64(0) / from_u64(0)
                if matches!(prim, crate::tir::PrimitiveType::I128) {
                    create_i128_literal(0, type_id, span)
                } else {
                    create_u128_literal(0, type_id, span)
                }
            }
            crate::tir::PrimitiveType::F32 => TirExpr::new(
                TirExprKind::FloatLiteral {
                    value: 0.0,
                    repr: "0.0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::F64 => TirExpr::new(
                TirExprKind::FloatLiteral {
                    value: 0.0,
                    repr: "0.0".to_string(),
                },
                type_id,
                span,
            ),
            crate::tir::PrimitiveType::Bool => {
                TirExpr::new(TirExprKind::BoolLiteral(false), type_id, span)
            }
            crate::tir::PrimitiveType::Char => {
                TirExpr::new(TirExprKind::CharLiteral('\0'), type_id, span)
            }
            crate::tir::PrimitiveType::V128 => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".to_string(),
                },
                type_id,
                span,
            ),
        },
        ResolvedType::Unit => TirExpr::new(TirExprKind::Unit, type_id, span),
        // For reference types (String, Array, struct, etc.), use null
        _ => TirExpr::new(TirExprKind::Null, type_id, span),
    }
}

/// Check if a type is a reference type (needs nullable Wasm type for lazy init)
fn is_reference_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Primitive(_) | ResolvedType::Unit | ResolvedType::Never => false,
        // Struct, Array, String, etc. are reference types in Wasm GC
        _ => true,
    }
}

/// Lower global variable initializers
///
/// For non-constant initializers, this:
/// 1. Replaces the initializer with a default value
/// 2. Generates a `__initialize_module` function containing the actual initialization
///
/// Note: The `__initialize_modules` function that calls all modules' `__initialize_module`
/// is generated in the post-processing step (see `generate_initialize_modules_flat`).
pub(super) fn lower_global_initializers(module: &mut TirModule) {
    let type_table = module.type_table.borrow();

    // Collect non-constant initializers with their indices for topological sorting
    let mut lazy_inits: Vec<(usize, String, ModuleSource, TypeId, TirExpr, Vec<TypeId>)> =
        Vec::new();

    for (idx, global) in module.globals.iter_mut().enumerate() {
        if !is_constant_initializer(&global.initializer) {
            // Save the original initializer with index and local types
            lazy_inits.push((
                idx,
                global.name.clone(),
                global.module_source.clone(),
                global.ty,
                global.initializer.clone(),
                global.local_types.clone(),
            ));
            // Replace with default value
            global.initializer = default_value_for_type(global.ty, &type_table, global.span);
            // Lazy-init globals must be Wasm-mutable (even if Wado-immutable)
            global.mutable = true;
            // Reference types need nullable Wasm type for lazy init
            if is_reference_type(global.ty, &type_table) {
                global.is_nullable = true;
            }
        }
    }

    drop(type_table);

    // If no lazy initializers, nothing to do
    if lazy_inits.is_empty() {
        return;
    }

    // Topologically sort the lazy initializers based on dependencies
    let sorted_inits = topological_sort_global_inits(&lazy_inits, &module.globals);

    // Generate __initialize_module function
    let span = Span::new(0, 0, 1, 1);
    let mut init_stmts: Vec<TirStmt> = Vec::new();
    let mut merged_local_types: Vec<TypeId> = Vec::new();

    for (_, name, module_source, _, mut initializer, local_types) in sorted_inits {
        // Renumber locals if this isn't the first global (to avoid index conflicts)
        let offset = u32::try_from(merged_local_types.len()).unwrap();
        if offset > 0 && !local_types.is_empty() {
            renumber_locals_in_expr(&mut initializer, offset);
        }
        merged_local_types.extend_from_slice(&local_types);

        // Create: global_name = initializer;
        let global_set = TirExpr::new(
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value: Box::new(initializer),
            },
            TypeTable::UNIT,
            span,
        );
        init_stmts.push(TirStmt::new(TirStmtKind::Expr(global_set), span));
    }

    let local_count = u32::try_from(merged_local_types.len()).unwrap();

    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    let init_func = TirFunction {
        module_source: module.module_source.clone(),
        is_async: false,
        name: "__initialize_module".to_string(),
        is_pub: true, // pub so it can be called from entry module's __initialize_modules
        is_export: false, // Internal function, not a world export
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: Vec::new(),
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(init_body),
        span,
        local_count,
        local_types: merged_local_types,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    };

    module.functions.push(Rc::new(RefCell::new(init_func)));
}

/// Collect global variable references from an expression
fn collect_global_refs(expr: &TirExpr, refs: &mut IndexSet<String>) {
    match &expr.kind {
        TirExprKind::GlobalVarGet { name, .. } => {
            refs.insert(name.clone());
        }
        // Recursively search in sub-expressions
        TirExprKind::Binary { left, right, .. } => {
            collect_global_refs(left, refs);
            collect_global_refs(right, refs);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_global_refs(inner, refs);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_global_refs(&arg.expr, refs);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_global_refs(receiver, refs);
            for arg in args {
                collect_global_refs(&arg.expr, refs);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_global_refs(&field.value, refs);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_global_refs(elem, refs);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
            ..
        } => {
            collect_global_refs(condition, refs);
            for stmt in &then_branch.stmts {
                if let TirStmtKind::Expr(e) = &stmt.kind {
                    collect_global_refs(e, refs);
                }
            }
            if let Some(else_blk) = else_branch {
                for stmt in &else_blk.stmts {
                    if let TirStmtKind::Expr(e) = &stmt.kind {
                        collect_global_refs(e, refs);
                    }
                }
            }
        }
        TirExprKind::Block(block) => {
            for stmt in &block.stmts {
                if let TirStmtKind::Expr(e) = &stmt.kind {
                    collect_global_refs(e, refs);
                }
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            collect_global_refs(inner, refs);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_global_refs(inner, refs);
            collect_global_refs(index, refs);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_global_refs(inner, refs);
        }
        TirExprKind::Assign { target, value } => {
            collect_global_refs(target, refs);
            collect_global_refs(value, refs);
        }
        // Leaf expressions - no sub-expressions
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. } => {}
        // Other expressions - skip for now
        _ => {}
    }
}

/// Topologically sort global initializers based on dependencies.
///
/// Returns the initializers in an order where dependencies are initialized first.
fn topological_sort_global_inits(
    lazy_inits: &[(usize, String, ModuleSource, TypeId, TirExpr, Vec<TypeId>)],
    _all_globals: &[TirGlobal],
) -> Vec<(usize, String, ModuleSource, TypeId, TirExpr, Vec<TypeId>)> {
    if lazy_inits.len() <= 1 {
        return lazy_inits.to_vec();
    }

    // Build a map from global name to its index in lazy_inits
    let name_to_idx: IndexMap<String, usize> = lazy_inits
        .iter()
        .enumerate()
        .map(|(i, (_, name, ..))| (name.clone(), i))
        .collect();

    // Build dependency graph: deps[i] = set of indices that i depends on
    let mut deps: Vec<IndexSet<usize>> = vec![IndexSet::default(); lazy_inits.len()];

    for (i, (_, _, _, _, initializer, _)) in lazy_inits.iter().enumerate() {
        let mut refs = IndexSet::default();
        collect_global_refs(initializer, &mut refs);

        for ref_name in refs {
            // Only consider dependencies on other lazy-init globals in this module
            if let Some(&dep_idx) = name_to_idx.get(&ref_name)
                && dep_idx != i
            {
                deps[i].insert(dep_idx);
            }
        }
    }

    // Kahn's algorithm for topological sort
    let mut in_degree: Vec<usize> = deps.iter().map(IndexSet::len).collect();
    let mut queue: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| i)
        .collect();

    let mut sorted = Vec::with_capacity(lazy_inits.len());

    while let Some(idx) = queue.pop_front() {
        sorted.push(lazy_inits[idx].clone());

        // Update dependents
        for (i, dep_set) in deps.iter().enumerate() {
            if dep_set.contains(&idx) {
                in_degree[i] -= 1;
                if in_degree[i] == 0 {
                    queue.push_back(i);
                }
            }
        }
    }

    // Check for cycles
    if sorted.len() < lazy_inits.len() {
        // Cycle detected - report which globals are involved
        let in_cycle: Vec<&str> = lazy_inits
            .iter()
            .enumerate()
            .filter(|(i, _)| in_degree[*i] > 0)
            .map(|(_, (_, name, ..))| name.as_str())
            .collect();
        panic!(
            "Circular dependency detected among global variables: {}",
            in_cycle.join(", ")
        );
    }

    sorted
}

/// Renumber all local variable indices in a TIR expression by adding an offset.
/// Used when merging multiple global initializers into a single `__initialize_module` function.
fn renumber_locals_in_expr(expr: &mut TirExpr, offset: u32) {
    match &mut expr.kind {
        TirExprKind::Local { index, .. } => *index += offset,
        TirExprKind::Binary { left, right, .. } => {
            renumber_locals_in_expr(left, offset);
            renumber_locals_in_expr(right, offset);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
            renumber_locals_in_expr(inner, offset);
        }
        TirExprKind::Index { expr: e, index: i } => {
            renumber_locals_in_expr(e, offset);
            renumber_locals_in_expr(i, offset);
        }
        TirExprKind::Assign { target, value } => {
            renumber_locals_in_expr(target, offset);
            renumber_locals_in_expr(value, offset);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                renumber_locals_in_expr(&mut arg.expr, offset);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                renumber_locals_in_expr(arg, offset);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            renumber_locals_in_expr(receiver, offset);
            for arg in args {
                renumber_locals_in_expr(&mut arg.expr, offset);
            }
        }
        TirExprKind::IndirectCall {
            callee: receiver,
            args,
        } => {
            renumber_locals_in_expr(receiver, offset);
            for arg in args {
                renumber_locals_in_expr(arg, offset);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                renumber_locals_in_expr(p, offset);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                renumber_locals_in_expr(&mut field.value, offset);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                renumber_locals_in_expr(elem, offset);
            }
        }
        TirExprKind::TupleSpread { expr } => {
            renumber_locals_in_expr(expr, offset);
        }
        TirExprKind::TypePackExpansion { call_expr, .. } => {
            renumber_locals_in_expr(call_expr, offset);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            renumber_locals_in_block(block, offset);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            renumber_locals_in_expr(condition, offset);
            renumber_locals_in_block(then_branch, offset);
            if let Some(eb) = else_branch {
                renumber_locals_in_block(eb, offset);
            }
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            renumber_locals_in_expr(scrutinee, offset);
            for arm in arms {
                renumber_locals_in_pattern(&mut arm.pattern, offset);
                if let Some(ref mut guard) = arm.guard {
                    renumber_locals_in_expr(guard, offset);
                }
                renumber_locals_in_expr(&mut arm.body, offset);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            renumber_locals_in_expr(value, offset);
        }
        // Leaf nodes with no locals
        _ => {}
    }
}

fn renumber_locals_in_block(block: &mut TirBlock, offset: u32) {
    for stmt in &mut block.stmts {
        renumber_locals_in_stmt(stmt, offset);
    }
}

fn renumber_locals_in_stmt(stmt: &mut TirStmt, offset: u32) {
    match &mut stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            *local_index += offset;
            renumber_locals_in_expr(value, offset);
        }
        TirStmtKind::Expr(expr) => renumber_locals_in_expr(expr, offset),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                renumber_locals_in_expr(v, offset);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            renumber_locals_in_expr(condition, offset);
            renumber_locals_in_block(then_block, offset);
            if let Some(eb) = else_block {
                renumber_locals_in_block(eb, offset);
            }
        }
        TirStmtKind::Loop { body } => renumber_locals_in_block(body, offset),
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                renumber_locals_in_expr(v, offset);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LabeledBlock { block, .. } => renumber_locals_in_block(block, offset),
        TirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => {
            renumber_locals_in_expr(scrutinee, offset);
            renumber_locals_in_pattern(pattern, offset);
            renumber_locals_in_block(then_block, offset);
            if let Some(eb) = else_block {
                renumber_locals_in_block(eb, offset);
            }
        }
        TirStmtKind::LetDestructure { pattern, value, .. } => {
            renumber_locals_in_pattern(pattern, offset);
            renumber_locals_in_expr(value, offset);
        }
        TirStmtKind::TaskReturn { .. } => {}
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn renumber_locals_in_pattern(pattern: &mut TirPattern, offset: u32) {
    match pattern {
        TirPattern::Binding { local_index, .. } => *local_index += offset,
        TirPattern::Tuple(patterns, _) => {
            for p in patterns {
                renumber_locals_in_pattern(p, offset);
            }
        }
        TirPattern::Variant { bindings, .. } => {
            for p in bindings {
                renumber_locals_in_pattern(p, offset);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for f in fields {
                renumber_locals_in_pattern(&mut f.pattern, offset);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
        TirPattern::Or(alternatives) => {
            for p in alternatives {
                renumber_locals_in_pattern(p, offset);
            }
        }
    }
}

/// Generate `__initialize_modules` for a `FlatPackage`.
pub(super) fn generate_initialize_modules_flat(flat: &mut FlatPackage) {
    let entry_source = flat.entry_module_source.clone();

    // Collect distinct module sources that have __initialize_module function
    let mut modules_with_init: Vec<ModuleSource> = Vec::new();
    let mut seen = IndexSet::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if func.name == "__initialize_module" && seen.insert(func.module_source.clone()) {
            modules_with_init.push(func.module_source.clone());
        }
    }

    if modules_with_init.is_empty() {
        return;
    }

    // Sort: entry module last
    modules_with_init.sort_by_key(|ms| i32::from(*ms == entry_source));

    let span = Span::new(0, 0, 1, 1);

    // Create __modules_initialized flag global
    let init_flag_global = TirGlobal {
        name: "__modules_initialized".to_string(),
        ty: TypeTable::BOOL,
        initializer: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, span),
        mutable: true,
        wado_mutable: true,
        is_pub: false,
        module_source: entry_source.clone(),
        span,
        is_nullable: false,
        local_types: Vec::new(),
    };
    flat.globals.push(init_flag_global);

    // Build __initialize_modules function body
    let mut init_stmts: Vec<TirStmt> = Vec::new();

    // Check flag: if __modules_initialized { return; }
    let flag_check = TirExpr::new(
        TirExprKind::GlobalVarGet {
            module_source: entry_source.clone(),
            name: "__modules_initialized".to_string(),
        },
        TypeTable::BOOL,
        span,
    );
    let early_return_stmt = TirStmt::new(TirStmtKind::Return { value: None }, span);
    let early_return_block = TirBlock {
        stmts: vec![early_return_stmt],
        span,
    };
    let if_already_init = TirStmt::new(
        TirStmtKind::If {
            condition: flag_check,
            then_block: early_return_block,
            else_block: None,
        },
        span,
    );
    init_stmts.push(if_already_init);

    // Call each module's __initialize_module
    for module_source in &modules_with_init {
        let call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: module_source.clone(),
                    name: "__initialize_module".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: Vec::new(),
                args: Vec::new(),
            },
            TypeTable::UNIT,
            span,
        );
        init_stmts.push(TirStmt::new(TirStmtKind::Expr(call), span));
    }

    // Set flag: __modules_initialized = true;
    let set_flag = TirExpr::new(
        TirExprKind::GlobalVarSet {
            module_source: entry_source.clone(),
            name: "__modules_initialized".to_string(),
            value: Box::new(TirExpr::new(
                TirExprKind::BoolLiteral(true),
                TypeTable::BOOL,
                span,
            )),
        },
        TypeTable::UNIT,
        span,
    );
    init_stmts.push(TirStmt::new(TirStmtKind::Expr(set_flag), span));

    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    let init_modules_func = TirFunction {
        module_source: entry_source.clone(),
        is_async: false,
        name: "__initialize_modules".to_string(),
        is_pub: false,
        is_export: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: None,
        params: Vec::new(),
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(init_body),
        span,
        local_count: 0,
        local_types: Vec::new(),
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_cm_export: false,
        is_ambient: false,
        inline_hint: InlineHint::Auto,
        comp_features: 0,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
    };

    flat.functions
        .push(Rc::new(RefCell::new(init_modules_func)));

    // Inject call to __initialize_modules at the start of entry point functions
    let init_call = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: entry_source.clone(),
                name: "__initialize_modules".to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: Vec::new(),
            args: Vec::new(),
        },
        TypeTable::UNIT,
        span,
    );
    let init_call_stmt = TirStmt::new(TirStmtKind::Expr(init_call), span);

    for func_rc in &flat.functions {
        let mut func = func_rc.borrow_mut();
        if func.module_source != entry_source {
            continue;
        }
        if func.is_export
            && let Some(ref mut body) = func.body
        {
            body.stmts.insert(0, init_call_stmt.clone());
        }
    }
}
