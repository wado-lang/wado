use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::flat_package::FlatPackage;
use crate::logger::{Bail, ErrorSink};
use crate::module_source::ModuleSource;
use crate::tir::FunctionRef;
use crate::tir::{
    FunctionKind, GlobalInit, InlineHint, ResolvedType, TirBinaryOp, TirBlock, TirExpr,
    TirExprKind, TirFunction, TirGlobal, TirLocal, TirPattern, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::token::Span;

use crate::lower::wide_int_literal::{create_i128_literal, create_u128_literal};

// `extract` and `build_initialize_modules` are the two halves of
// the global-initializer planner. They run at different points in
// `super::plan` (extract before boxing, build_initialize_modules
// after closure), so they cannot share a single entry point. The
// `extract` half emits per-module init functions; the
// `build_initialize_modules` half combines them into the top-level
// `__initialize_modules` aggregator.

/// Whether the Wasm slot can hold this value directly, so the global needs no
/// assignment from an initialization function.
///
/// Deliberately under-approximates what a constant expression can express: an
/// aggregate only becomes a `struct.new` once the optimizer has collapsed the
/// builder producing it, which is not knowable here. The classifier on the
/// lowered Wasm value promotes back what this defers. Decidable here is what
/// is already a value: a literal, and arithmetic over literals.
fn is_constant_initializer(expr: &TirExpr, type_table: &TypeTable) -> bool {
    match &expr.kind {
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit => true,
        TirExprKind::Cast { expr: inner, .. } => is_constant_initializer(inner, type_table),
        TirExprKind::Unary { op, expr: inner } => {
            // Negation of literals is constant
            matches!(op, TirUnaryOp::Neg) && is_constant_initializer(inner, type_table)
        }
        TirExprKind::Binary { op, left, right } => {
            matches!(op, TirBinaryOp::Add | TirBinaryOp::Sub | TirBinaryOp::Mul)
                && is_wasm_width_int(expr.type_id, type_table)
                && is_constant_initializer(left, type_table)
                && is_constant_initializer(right, type_table)
        }
        _ => false,
    }
}

/// An integer whose Wado width matches the Wasm operand it lowers to, so
/// wrapping needs no masking. Wasm admits constant `add` / `sub` / `mul` on
/// `i32` and `i64` only — never a narrower integer, and never a float.
fn is_wasm_width_int(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Primitive(
            crate::tir::PrimitiveType::I32
                | crate::tir::PrimitiveType::U32
                | crate::tir::PrimitiveType::I64
                | crate::tir::PrimitiveType::U64
        )
    )
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
                    create_i128_literal(0, type_id, type_table, span)
                } else {
                    create_u128_literal(0, type_id, type_table, span)
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
        // For reference types (String, List, struct, etc.), use null
        _ => TirExpr::new(TirExprKind::Null, type_id, span),
    }
}

/// Extract non-constant global initializers into a per-module
/// `__initialize_module` function (one per source module; the
/// functions share a name and are disambiguated by their
/// `module_source` field). For each lazy-init global the original
/// initializer is replaced with a default value, and the original
/// expression is moved into the module's `__initialize_module` body.
///
/// Must run before `boxing` because the extracted initializer code
/// may contain `&primitive` / closure expressions that boxing /
/// closure rewrite. The top-level `__initialize_modules` aggregator
/// that calls each module's `__initialize_module` is built later by
/// [`build_initialize_modules`].
pub fn extract(flat: &mut FlatPackage, errors: &dyn ErrorSink) -> Result<(), Bail> {
    let type_table = flat.type_table.borrow();

    // Collect non-constant initializers with their indices for topological sorting
    let mut lazy_inits: Vec<(usize, String, ModuleSource, TypeId, TirExpr, Vec<TirLocal>)> =
        Vec::new();

    for (idx, global) in flat.globals.iter_mut().enumerate() {
        if is_constant_initializer(global.init.slot_expr(), &type_table) {
            continue;
        }
        // The declared value moves into the initialization function rather
        // than being copied, so the global cannot claim a value it no longer
        // holds.
        let placeholder = default_value_for_type(global.ty, &type_table, global.span);
        let GlobalInit::Direct(declared) =
            std::mem::replace(&mut global.init, GlobalInit::Deferred(placeholder))
        else {
            panic!("a global is Direct until this pass defers it");
        };
        lazy_inits.push((
            idx,
            global.name.clone(),
            global.module_source.clone(),
            global.ty,
            declared,
            global.locals.clone(),
        ));
    }

    drop(type_table);

    // If no lazy initializers, nothing to do
    if lazy_inits.is_empty() {
        return Ok(());
    }

    // Partition lazy inits by their owning module so each module gets
    // its own `__initialize_module` function. Insertion order is
    // preserved so cross-module sibling ordering matches the original
    // global declaration order, which the aggregator then walks in
    // entry-last order (see `build_initialize_modules`).
    let mut by_module: IndexMap<ModuleSource, Vec<_>> = IndexMap::default();
    for entry in lazy_inits {
        by_module.entry(entry.2.clone()).or_default().push(entry);
    }

    let reads_by_function = global_reads_by_function(&flat.functions);
    let span = Span::new(0, 0, 1, 1);
    for (module_source, module_inits) in by_module {
        let sorted_inits =
            topological_sort_global_inits(&module_inits, &reads_by_function, errors)?;
        let init_func = build_module_init_function(module_source, sorted_inits, span);
        flat.functions.push(Rc::new(RefCell::new(init_func)));
    }
    Ok(())
}

fn build_module_init_function(
    module_source: ModuleSource,
    sorted_inits: Vec<(usize, String, ModuleSource, TypeId, TirExpr, Vec<TirLocal>)>,
    span: Span,
) -> TirFunction {
    let mut init_stmts: Vec<TirStmt> = Vec::new();
    let mut merged_locals: Vec<TirLocal> = Vec::new();

    for (_, name, gvs_module_source, _, mut initializer, locals) in sorted_inits {
        let offset = u32::try_from(merged_locals.len()).unwrap();
        if offset > 0 && !locals.is_empty() {
            renumber_locals_in_expr(&mut initializer, offset);
        }
        merged_locals.extend(locals);

        let global_set = TirExpr::new(
            TirExprKind::GlobalVarSet {
                module_source: gvs_module_source,
                name,
                value: Box::new(initializer),
            },
            TypeTable::UNIT,
            span,
        );
        init_stmts.push(TirStmt::new(TirStmtKind::Expr(global_set), span));
    }

    let local_count = u32::try_from(merged_locals.len()).unwrap();
    let init_body = TirBlock {
        stmts: init_stmts,
        span,
    };

    TirFunction {
        module_source,
        is_async: false,
        name: crate::name::MODULE_INIT_FUNCTION.to_string(),
        visibility: crate::ast::Visibility::Public,
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
        local_count,
        locals: merged_locals,
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
        kind: FunctionKind::Regular,
        return_abi: crate::tir::ReturnAbi::default(),
    }
}

/// What a function body reads and calls, in one walk.
///
/// Globals are keyed by `(module_source, name)` so two modules each declaring a
/// global with the same name don't collide in the dependency graph; callees by
/// [`FunctionRef::full_name`], the same key [`global_reads_by_function`] maps.
#[derive(Default)]
struct BodyReads {
    globals: IndexSet<(ModuleSource, String)>,
    callees: IndexSet<String>,
}

impl crate::tir_visitor::TirRefVisitor for BodyReads {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::GlobalVarGet {
                name,
                module_source,
            } => {
                self.globals.insert((module_source.clone(), name.clone()));
            }
            TirExprKind::Call { func, .. } => {
                self.callees.insert(func.full_name());
            }
            // A function named as a value reaches a call this walk cannot see —
            // `apply(reader)` calls `reader` through a parameter, and a closure
            // body is a function of its own. Counting the mention keeps what it
            // reads on the mentioning side of the graph.
            TirExprKind::FuncRef {
                module_source,
                name,
                ..
            } => {
                self.callees.insert(function_key(module_source, name));
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// The key [`global_reads_by_function`] maps a function under: the same string
/// [`FunctionRef::full_name`] produces for a free function, so a call and a
/// bare mention of the same function land on one entry.
fn function_key(module_source: &ModuleSource, name: &str) -> String {
    FunctionRef {
        module_source: module_source.clone(),
        name: name.to_string(),
        monomorph_info: None,
        method_info: None,
    }
    .full_name()
}

/// Every global each function reads, closed over the call graph.
///
/// An initializer's dependency is not always written in it: a call reads
/// globals the caller never names, and ordering on the directly-written
/// references alone leaves such a global still holding its placeholder when the
/// caller runs. Computed as a least fixpoint, so a recursive cycle contributes
/// each member's own reads and terminates.
fn global_reads_by_function(
    functions: &[Rc<RefCell<TirFunction>>],
) -> IndexMap<String, IndexSet<(ModuleSource, String)>> {
    use crate::tir_visitor::TirRefVisitor;

    let mut reads: IndexMap<String, IndexSet<(ModuleSource, String)>> = IndexMap::default();
    let mut callees: IndexMap<String, IndexSet<String>> = IndexMap::default();
    for func_rc in functions {
        let func = func_rc.borrow();
        let Some(body) = func.body.as_ref() else {
            continue;
        };
        let mut scan = BodyReads::default();
        scan.visit_block(body);
        let key = FunctionRef::from_resolved(&func, func.module_source.clone()).full_name();
        reads.entry(key.clone()).or_default().extend(scan.globals);
        callees.entry(key).or_default().extend(scan.callees);
    }

    loop {
        let mut grown: Vec<(String, IndexSet<(ModuleSource, String)>)> = Vec::new();
        for (name, called) in &callees {
            let known = reads.get(name);
            let mut fresh = IndexSet::default();
            for callee in called {
                let Some(callee_reads) = reads.get(callee) else {
                    continue;
                };
                for global in callee_reads {
                    if known.is_none_or(|k| !k.contains(global)) {
                        fresh.insert(global.clone());
                    }
                }
            }
            if !fresh.is_empty() {
                grown.push((name.clone(), fresh));
            }
        }
        if grown.is_empty() {
            return reads;
        }
        for (name, fresh) in grown {
            reads.entry(name).or_default().extend(fresh);
        }
    }
}

/// The globals an initializer depends on, kept apart by how certainly: those it
/// names, and those the functions it reaches read.
#[derive(Default)]
struct InitRefs {
    direct: IndexSet<(ModuleSource, String)>,
    via_calls: IndexSet<(ModuleSource, String)>,
}

fn collect_global_refs(
    expr: &TirExpr,
    reads_by_function: &IndexMap<String, IndexSet<(ModuleSource, String)>>,
) -> InitRefs {
    use crate::tir_visitor::TirRefVisitor;

    let mut scan = BodyReads::default();
    scan.visit_expr(expr);
    let mut refs = InitRefs {
        direct: scan.globals,
        via_calls: IndexSet::default(),
    };
    for callee in &scan.callees {
        let Some(callee_reads) = reads_by_function.get(callee) else {
            continue;
        };
        for global in callee_reads {
            if !refs.direct.contains(global) {
                refs.via_calls.insert(global.clone());
            }
        }
    }
    refs
}

/// Whether `from` already depends on `to`, directly or transitively — so an
/// edge `to → from` would close a cycle.
fn depends_on(deps: &[IndexSet<usize>], from: usize, to: usize) -> bool {
    let mut seen = vec![false; deps.len()];
    let mut stack = vec![from];
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if std::mem::replace(&mut seen[node], true) {
            continue;
        }
        stack.extend(deps[node].iter().copied());
    }
    false
}

/// Topologically sort global initializers based on dependencies.
///
/// Returns the initializers in an order where dependencies are initialized first.
fn topological_sort_global_inits(
    lazy_inits: &[(usize, String, ModuleSource, TypeId, TirExpr, Vec<TirLocal>)],
    reads_by_function: &IndexMap<String, IndexSet<(ModuleSource, String)>>,
    errors: &dyn ErrorSink,
) -> Result<Vec<(usize, String, ModuleSource, TypeId, TirExpr, Vec<TirLocal>)>, Bail> {
    if lazy_inits.len() <= 1 {
        return Ok(lazy_inits.to_vec());
    }

    // Build a map from `(module_source, name)` to its index in
    // lazy_inits. The compound key keeps cross-module same-named
    // globals separated when both happen to share a topo-sort input
    // (today the planner partitions by module, but the keying is
    // defensive against any future re-merge).
    let key_to_idx: IndexMap<(ModuleSource, String), usize> = lazy_inits
        .iter()
        .enumerate()
        .map(|(i, (_, name, module_source, ..))| ((module_source.clone(), name.clone()), i))
        .collect();

    // Build dependency graph: deps[i] = set of indices that i depends on.
    let mut deps: Vec<IndexSet<usize>> = vec![IndexSet::default(); lazy_inits.len()];
    let edges = |refs: IndexSet<(ModuleSource, String)>, i: usize| -> Vec<usize> {
        refs.into_iter()
            .filter_map(|key| key_to_idx.get(&key).copied())
            .filter(|&dep| dep != i)
            .collect()
    };

    let scanned: Vec<InitRefs> = lazy_inits
        .iter()
        .map(|(_, _, _, _, initializer, _)| collect_global_refs(initializer, reads_by_function))
        .collect();

    // A reference written in the initializer is a definite dependency.
    for (i, refs) in scanned.iter().enumerate() {
        for dep in edges(refs.direct.clone(), i) {
            deps[i].insert(dep);
        }
    }
    // What a callee reads is inferred, and the inference is path-insensitive: a
    // helper that reads two globals makes each initializer calling it look
    // dependent on the other, even when no execution reads both. Such an edge
    // yields rather than manufacturing a cycle out of a program that has none —
    // the definite edges above already fix every order that is really required.
    for (i, refs) in scanned.iter().enumerate() {
        for dep in edges(refs.via_calls.clone(), i) {
            if !depends_on(&deps, dep, i) {
                deps[i].insert(dep);
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

    if sorted.len() < lazy_inits.len() {
        let cycle: Vec<&(usize, String, ModuleSource, TypeId, TirExpr, Vec<TirLocal>)> = lazy_inits
            .iter()
            .enumerate()
            .filter(|(i, _)| in_degree[*i] > 0)
            .map(|(_, init)| init)
            .collect();
        let names: Vec<&str> = cycle.iter().map(|(_, name, ..)| name.as_str()).collect();
        let (_, _, module_source, _, initializer, _) = cycle[0];
        return Err(errors.fatal_in(
            module_source,
            Diagnostic {
                severity: Severity::Error,
                code: Code::CircularDependency,
                message: format!(
                    "global initializers form a cycle: {}. Each waits for a value \
                     another has not been given yet, so none can go first.",
                    names.join(", ")
                ),
                span: Some(DiagnosticSpan::from_span(&initializer.span, None)),
            },
        ));
    }

    Ok(sorted)
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
        | TirExprKind::VariantPayload { expr: inner, .. } => {
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

/// Order the per-module initializers so each runs after the modules whose
/// globals it reads, entry last.
///
/// [`topological_sort_global_inits`] settles the order within a module; a
/// global read across a module boundary needs the same treatment one level up,
/// or the reader finds a placeholder. Discovery order is no substitute: it
/// tracks how the loader happened to reach the modules.
fn sort_modules_by_dependency(
    modules: &mut Vec<ModuleSource>,
    entry_source: &ModuleSource,
    functions: &[Rc<RefCell<TirFunction>>],
) {
    let reads = global_reads_by_function(functions);
    let module_deps = |module: &ModuleSource| -> IndexSet<ModuleSource> {
        let key = function_key(module, crate::name::MODULE_INIT_FUNCTION);
        reads
            .get(&key)
            .into_iter()
            .flatten()
            .map(|(source, _)| source.clone())
            .filter(|source| source != module)
            .collect()
    };

    let mut ordered: Vec<ModuleSource> = Vec::with_capacity(modules.len());
    let mut placed: IndexSet<ModuleSource> = IndexSet::default();
    // Entry last: it is the one module every other is linked into, and a cycle
    // among the rest — which imports cannot form — would otherwise strand it.
    let mut pending: Vec<ModuleSource> = modules
        .iter()
        .filter(|ms| *ms != entry_source)
        .cloned()
        .collect();

    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|ms| module_deps(ms).iter().all(|dep| placed.contains(dep)));
        // No module is ready only if the remainder depends on each other, which
        // the import graph cannot express. Take one so the loop terminates.
        let next = pending.remove(ready.unwrap_or(0));
        placed.insert(next.clone());
        ordered.push(next);
    }
    if modules.iter().any(|ms| ms == entry_source) {
        ordered.push(entry_source.clone());
    }
    *modules = ordered;
}

/// Generate `__initialize_modules` for a `FlatPackage`.
/// Generate the top-level `__initialize_modules` aggregator. Must run
/// after all per-module init functions exist (i.e. after [`extract`]).
pub fn build_initialize_modules(flat: &mut FlatPackage) {
    let entry_source = flat.entry_module_source.clone();

    // Collect distinct module sources that have __initialize_module function
    let mut modules_with_init: Vec<ModuleSource> = Vec::new();
    let mut seen = IndexSet::default();
    for func_rc in &flat.functions {
        let func = func_rc.borrow();
        if func.name == crate::name::MODULE_INIT_FUNCTION && seen.insert(func.module_source.clone())
        {
            modules_with_init.push(func.module_source.clone());
        }
    }

    if modules_with_init.is_empty() {
        return;
    }

    sort_modules_by_dependency(&mut modules_with_init, &entry_source, &flat.functions);

    let span = Span::new(0, 0, 1, 1);

    // Create __modules_initialized flag global
    let init_flag_global = TirGlobal {
        name: "__modules_initialized".to_string(),
        ty: TypeTable::BOOL,
        init: GlobalInit::Direct(TirExpr::new(
            TirExprKind::BoolLiteral(false),
            TypeTable::BOOL,
            span,
        )),
        param: None,
        wado_mutable: true,
        visibility: crate::ast::Visibility::Private,
        module_source: entry_source.clone(),
        span,
        locals: Vec::new(),
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

    // The fall-through below the guard runs once per program: mark it cold so
    // the guard is hinted likely-taken (`hint_guard_fall_through`), including
    // the copies inlined into each export entry, and the inliner excludes the
    // one-shot init calls from its cost estimate.
    init_stmts.push(TirStmt::new(
        TirStmtKind::Expr(crate::synthesis::common::builtin_call(
            "cold_path",
            Vec::new(),
            TypeTable::UNIT,
        )),
        span,
    ));

    // Call each module's __initialize_module
    for module_source in &modules_with_init {
        let call = TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef {
                    module_source: module_source.clone(),
                    name: crate::name::MODULE_INIT_FUNCTION.to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                type_args: Vec::new(),
                args: Vec::new(),
                has_receiver: false,
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
        name: crate::name::MODULES_INIT_FUNCTION.to_string(),
        visibility: crate::ast::Visibility::Private,
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
        locals: Vec::new(),
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
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    };

    flat.functions
        .push(Rc::new(RefCell::new(init_modules_func)));

    // Inject call to __initialize_modules at the start of entry point functions
    let init_call = TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: entry_source.clone(),
                name: crate::name::MODULES_INIT_FUNCTION.to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: Vec::new(),
            args: Vec::new(),
            has_receiver: false,
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
