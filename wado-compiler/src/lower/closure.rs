use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::name::{LocalMethodName, MethodName, ModuleSource};
use crate::tir::{
    CallArg, ClosureFunctor, FunctionKind, FunctionRef, InlineHint, ResolvedType, TirBlock,
    TirCapture, TirExpr, TirExprKind, TirField, TirFunction, TirImpl, TirLocal, TirMatchArm,
    TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField,
    TirStructPatternField, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Information about a closure collected during the first pass
#[derive(Debug, Clone)]
struct CollectedClosure {
    /// Unique closure ID (assigned in order of collection)
    id: u32,
    /// Parameters of the closure
    params: Vec<(String, TypeId)>,
    /// The closure body expression (cloned for __call method generation)
    body: TirExpr,
    /// Captures from the closure
    captures: Vec<TirCapture>,
    /// Return type of the closure
    return_type: TypeId,
    /// Original function type (for compatibility)
    func_type_id: TypeId,
    /// Span of the original closure
    span: Span,
}

// FunctorInfo moved to tir::ClosureFunctor

/// Function signature info for converting `FuncRef` to Closure.
struct FuncSig {
    params: Vec<(String, TypeId)>,
    return_type: TypeId,
}

/// Key for fn-param specialization: (callee function name, parameter index -> functor type ID)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FnParamSpecKey {
    /// Name of the callee function
    callee_name: String,
    /// Map from parameter index to functor struct type ID (for fn-type params with closure args)
    functor_types: Vec<(u32, TypeId)>,
}

/// Lowers closures to functor structs with `__call` methods.
///
/// For each closure, this generates:
/// 1. A synthetic struct `__Closure_N` with fields for captured variables
/// 2. A `__call` method containing the transformed closure body
///
/// Transformations (selective - only for closures stored in locals and called directly):
/// - `Closure { params, body, captures }` → `StructLiteral { __Closure_N, capture_values }`
/// - `Capture { index }` (in body) → `FieldAccess { self, __capture_{index} }`
/// - `IndirectCall { callee, args }` (known closure) → `MethodCall { callee, __call, args }`
///
/// Closures passed as function arguments are transformed via fn-param specialization:
/// - A specialized version of the callee is generated with functor struct params
/// - The call is updated to use the specialized function with `StructLiteral` args
pub(super) struct ClosureLowerer {
    /// Sequential ID assigned to each `Closure` node during the pre-pass.
    /// Each Closure's `functor_id` is set once and read by all later passes;
    /// no counter is re-walked in lockstep with the AST.
    next_closure_id: u32,
    /// Module source for generated items
    module_source: ModuleSource,
    /// Collected closures during first pass
    collected_closures: Vec<CollectedClosure>,
    /// Generated functor information (populated after struct/method generation)
    /// These will be stored in `module.closure_functors` for the optimizer
    functor_infos: Vec<ClosureFunctor>,
    /// Map from local variable index to closure ID (for tracking closures stored in locals)
    local_to_closure: IndexMap<u32, u32>,
    /// Closure IDs that can be specialized (stored in locals, called directly).
    /// Non-specializable closures use `ClosureToCanonical` for type-erased representation.
    specializable: IndexSet<u32>,
    /// Generated structs to add to module
    generated_structs: Vec<TirStruct>,
    /// Generated functions to add to module
    generated_functions: Vec<Rc<RefCell<TirFunction>>>,
    /// Map from fn-param spec key to specialized function name
    fn_param_specializations: IndexMap<FnParamSpecKey, String>,
}

impl ClosureLowerer {
    pub(super) fn new(module_source: &ModuleSource) -> Self {
        Self {
            next_closure_id: 0,
            module_source: module_source.clone(),
            collected_closures: Vec::new(),
            functor_infos: Vec::new(),
            local_to_closure: IndexMap::default(),
            specializable: IndexSet::default(),
            generated_structs: Vec::new(),
            generated_functions: Vec::new(),
            fn_param_specializations: IndexMap::default(),
        }
    }

    /// Lower all closures in a module
    pub(super) fn lower_module(&mut self, module: &mut TirModule) {
        // Phase 0: Convert FuncRef used as values to zero-capture Closures.
        // Named functions used as values (e.g., `&double` or `double` passed to fn-type params)
        // need to become Closure nodes so the existing closure pipeline handles them.
        self.convert_func_refs_to_closures(module);

        // Phase 1: collect all closures and assign each a stable `functor_id`.
        //
        // The ID is written onto the `Closure` expression node itself, so every
        // later pass reads it directly off the AST. This avoids fragile
        // counter walks that have to re-traverse the module in lockstep with
        // the original collection order — counters break the moment we
        // introduce additional walks (e.g. the generated `__call` methods) or
        // skip a sub-tree.
        self.next_closure_id = 0;
        self.collected_closures.clear();

        let func_refs: Vec<_> = module.functions.clone();
        for func_rc in &func_refs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                self.collect_closures_in_block(body);
            }
        }

        // Also collect from impl methods
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    self.collect_closures_in_block(body);
                }
            }
        }

        // Generate functor structs and __call methods. Each `__call` body is
        // a clone of the closure body taken AFTER `collect_closures_in_block`
        // assigned IDs to nested closures, so those clones carry the same
        // stable functor IDs as the originals.
        self.generate_functor_items(&mut module.type_table.borrow_mut());

        // Phase 2: analyse which closures are safe to specialise. Reads
        // `functor_id` directly off the Closure node — no counter.
        for func_rc in &func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.local_to_closure.clear();
                self.analyze_closure_safety_block(body);
            }
        }
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.local_to_closure.clear();
                    self.analyze_closure_safety_block(body);
                }
            }
        }

        // Phase 2.5: Generate specialized functions for fn-param monomorphization.
        // For closures passed as fn-type arguments, generate specialized callees.
        self.generate_fn_param_specializations(
            &func_refs,
            &module.impls,
            &mut module.type_table.borrow_mut(),
        );

        // Phase 3: transform closures to struct literals and IndirectCall to MethodCall.
        //
        // We walk the generated `__call` methods alongside the original
        // module functions: a nested closure's body lives in its parent
        // closure's __call method (cloned in by `transform_closure_body`),
        // and that copy must be lowered too. Each function uses a fresh
        // `local_to_closure` map (keyed by per-function local indices).
        // `transform_expr` deliberately does NOT recurse into Closure
        // bodies — those are visited via the corresponding __call method.
        let lowered_funcs: Vec<_> = func_refs
            .iter()
            .cloned()
            .chain(self.generated_functions.iter().cloned())
            .collect();
        for func_rc in &lowered_funcs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                self.local_to_closure.clear();
                self.transform_block(body, &mut module.type_table.borrow_mut());
            }
            self.update_local_types(&mut func);
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    self.local_to_closure.clear();
                    self.transform_block(body, &mut module.type_table.borrow_mut());
                }
                self.update_local_types(method);
            }
        }

        // Phase 4: transform any remaining Closure nodes to ClosureToCanonical.
        // These are closures that weren't specialised (fn-param stored in
        // struct field). Walks the same set as phase 3 so cloned bodies in
        // `__call` methods are also covered.
        for func_rc in &lowered_funcs {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                self.transform_remaining_closures_block(body);
            }
        }
        for impl_block in &mut module.impls {
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    self.transform_remaining_closures_block(body);
                }
            }
        }

        // Store functor metadata in module for the optimizer to use.
        // This enables closure inlining by providing the __call method body.
        module.closure_functors = std::mem::take(&mut self.functor_infos);

        // Add ALL generated structs and functions to the module
        module
            .structs
            .extend(std::mem::take(&mut self.generated_structs));
        module
            .functions
            .extend(std::mem::take(&mut self.generated_functions));
    }

    /// Convert `FuncRef` nodes (used as values, not in Call/MethodCall func positions) to
    /// zero-capture `Closure` nodes. This enables named functions to be passed as function-type
    /// arguments (e.g., `apply(double, 21)` or `apply(&double, 21)`).
    fn convert_func_refs_to_closures(&self, module: &mut TirModule) {
        // Build a map from function name to (param_types, return_type)
        let mut func_sigs: IndexMap<String, FuncSig> = IndexMap::default();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            func_sigs.insert(
                func.name.clone(),
                FuncSig {
                    params: func
                        .params
                        .iter()
                        .map(|p| (p.name.clone(), p.type_id))
                        .collect(),
                    return_type: func.return_type,
                },
            );
        }
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                func_sigs.insert(
                    method.name.clone(),
                    FuncSig {
                        params: method
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), p.type_id))
                            .collect(),
                        return_type: method.return_type,
                    },
                );
            }
        }

        let mut type_table = module.type_table.borrow_mut();
        let module_source = module.module_source.clone();

        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                Self::convert_func_refs_in_block(body, &func_sigs, &mut type_table, &module_source);
            }
        }
        drop(type_table);
        for impl_block in &mut module.impls {
            let mut type_table = module.type_table.borrow_mut();
            for method in &mut impl_block.methods {
                if let Some(body) = &mut method.body {
                    Self::convert_func_refs_in_block(
                        body,
                        &func_sigs,
                        &mut type_table,
                        &module_source,
                    );
                }
            }
        }
    }

    fn convert_func_refs_in_block(
        block: &mut TirBlock,
        func_sigs: &IndexMap<String, FuncSig>,
        type_table: &mut TypeTable,
        module_source: &ModuleSource,
    ) {
        for stmt in &mut block.stmts {
            Self::convert_func_refs_in_stmt(stmt, func_sigs, type_table, module_source);
        }
    }

    fn convert_func_refs_in_stmt(
        stmt: &mut TirStmt,
        func_sigs: &IndexMap<String, FuncSig>,
        type_table: &mut TypeTable,
        module_source: &ModuleSource,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
                Self::convert_func_refs_in_expr(value, func_sigs, type_table, module_source);
            }
            TirStmtKind::Return { value } => {
                if let Some(v) = value {
                    Self::convert_func_refs_in_expr(v, func_sigs, type_table, module_source);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::convert_func_refs_in_expr(condition, func_sigs, type_table, module_source);
                Self::convert_func_refs_in_block(then_block, func_sigs, type_table, module_source);
                if let Some(else_blk) = else_block {
                    Self::convert_func_refs_in_block(
                        else_blk,
                        func_sigs,
                        type_table,
                        module_source,
                    );
                }
            }
            TirStmtKind::Loop { body, .. } => {
                Self::convert_func_refs_in_block(body, func_sigs, type_table, module_source);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    Self::convert_func_refs_in_expr(v, func_sigs, type_table, module_source);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                Self::convert_func_refs_in_block(block, func_sigs, type_table, module_source);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                Self::convert_func_refs_in_expr(scrutinee, func_sigs, type_table, module_source);
                Self::convert_func_refs_in_block(then_block, func_sigs, type_table, module_source);
                if let Some(else_blk) = else_block {
                    Self::convert_func_refs_in_block(
                        else_blk,
                        func_sigs,
                        type_table,
                        module_source,
                    );
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                Self::convert_func_refs_in_expr(value, func_sigs, type_table, module_source);
            }
            TirStmtKind::TaskReturn { .. } | TirStmtKind::VariadicForOf { .. } => {}
        }
    }

    /// Convert a `FuncRef` expression (or `Ref(FuncRef)`) to a zero-capture `Closure`.
    fn convert_func_refs_in_expr(
        expr: &mut TirExpr,
        func_sigs: &IndexMap<String, FuncSig>,
        type_table: &mut TypeTable,
        module_source: &ModuleSource,
    ) {
        // Handle `&FuncRef` → collapse to Closure (since &fn(...) = fn(...) for GC types)
        if let TirExprKind::Unary {
            op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
            expr: inner,
        } = &mut expr.kind
            && matches!(inner.kind, TirExprKind::FuncRef { .. })
        {
            Self::convert_func_refs_in_expr(inner, func_sigs, type_table, module_source);
            // If inner was converted to a Closure, collapse the Ref wrapper.
            // Function values are GC references, so &fn(...) = fn(...).
            if matches!(inner.kind, TirExprKind::Closure { .. }) {
                let inner_owned = std::mem::replace(
                    inner.as_mut(),
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                *expr = inner_owned;
            }
            return;
        }

        // Convert FuncRef to Closure
        if let TirExprKind::FuncRef {
            name,
            module_source: func_module,
        } = &expr.kind
        {
            if let Some(sig) = func_sigs.get(name.as_str()) {
                let span = expr.span;
                let func_name = name.clone();
                let func_module = func_module.clone();

                // Build closure params (synthetic names)
                let closure_params: Vec<(String, TypeId)> = sig
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, (_, ty))| (format!("__fn_ref_p{i}"), *ty))
                    .collect();

                // Build call args: Local references to each closure param
                let call_args: Vec<CallArg> = closure_params
                    .iter()
                    .enumerate()
                    .map(|(i, (name, ty))| {
                        CallArg::new(
                            TirExpr::new(
                                TirExprKind::Local {
                                    index: i as u32,
                                    name: name.clone(),
                                },
                                *ty,
                                span,
                            ),
                            false,
                        )
                    })
                    .collect();

                // Build the body: Call to the original function
                let body = TirExpr::new(
                    TirExprKind::Call {
                        func: FunctionRef {
                            module_source: func_module,
                            name: func_name,
                            monomorph_info: None,
                            method_info: None,
                        },
                        type_args: Vec::new(),
                        args: call_args,
                    },
                    sig.return_type,
                    span,
                );

                // Build the function type
                let param_types: Vec<TypeId> = closure_params.iter().map(|(_, t)| *t).collect();
                let func_type =
                    type_table.make_function(param_types, sig.return_type, Vec::new(), Vec::new());

                // Replace the FuncRef with a Closure. The synthetic body
                // is a single `Call` that forwards to the named function;
                // the closure's local namespace is exactly its parameters.
                expr.kind = TirExprKind::Closure {
                    params: closure_params,
                    body: Box::new(body),
                    captures: Vec::new(),
                    functor_id: None,
                    source_text: None,
                    address_taken_locals: crate::hashmap::IndexSet::default(),
                    body_locals: Vec::new(),
                };
                expr.type_id = func_type;
            }
            return;
        }

        // Recurse into sub-expressions (but NOT into Call.func / MethodCall.func which are
        // FunctionRef, not TirExprKind::FuncRef)
        match &mut expr.kind {
            TirExprKind::Binary { left, right, .. } => {
                Self::convert_func_refs_in_expr(left, func_sigs, type_table, module_source);
                Self::convert_func_refs_in_expr(right, func_sigs, type_table, module_source);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                Self::convert_func_refs_in_expr(inner, func_sigs, type_table, module_source);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::convert_func_refs_in_expr(
                        &mut arg.expr,
                        func_sigs,
                        type_table,
                        module_source,
                    );
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    Self::convert_func_refs_in_expr(arg, func_sigs, type_table, module_source);
                }
            }
            TirExprKind::MethodCall {
                receiver, args, ..
            } => {
                Self::convert_func_refs_in_expr(receiver, func_sigs, type_table, module_source);
                for arg in args {
                    Self::convert_func_refs_in_expr(
                        &mut arg.expr,
                        func_sigs,
                        type_table,
                        module_source,
                    );
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::convert_func_refs_in_expr(callee, func_sigs, type_table, module_source);
                for arg in args {
                    Self::convert_func_refs_in_expr(arg, func_sigs, type_table, module_source);
                }
            }
            TirExprKind::Closure { body, .. } => {
                Self::convert_func_refs_in_expr(body, func_sigs, type_table, module_source);
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                Self::convert_func_refs_in_expr(functor, func_sigs, type_table, module_source);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::convert_func_refs_in_expr(
                        &mut field.value,
                        func_sigs,
                        type_table,
                        module_source,
                    );
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::convert_func_refs_in_expr(elem, func_sigs, type_table, module_source);
                }
            }
            TirExprKind::Assign { target, value } => {
                Self::convert_func_refs_in_expr(target, func_sigs, type_table, module_source);
                Self::convert_func_refs_in_expr(value, func_sigs, type_table, module_source);
            }
            TirExprKind::Index {
                expr: array,
                index,
            } => {
                Self::convert_func_refs_in_expr(array, func_sigs, type_table, module_source);
                Self::convert_func_refs_in_expr(index, func_sigs, type_table, module_source);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::convert_func_refs_in_expr(condition, func_sigs, type_table, module_source);
                Self::convert_func_refs_in_block(
                    then_branch,
                    func_sigs,
                    type_table,
                    module_source,
                );
                if let Some(else_blk) = else_branch {
                    Self::convert_func_refs_in_block(
                        else_blk,
                        func_sigs,
                        type_table,
                        module_source,
                    );
                }
            }
            TirExprKind::Block(block) => {
                Self::convert_func_refs_in_block(block, func_sigs, type_table, module_source);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                Self::convert_func_refs_in_block(block, func_sigs, type_table, module_source);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::convert_func_refs_in_expr(scrutinee, func_sigs, type_table, module_source);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        Self::convert_func_refs_in_expr(
                            guard,
                            func_sigs,
                            type_table,
                            module_source,
                        );
                    }
                    Self::convert_func_refs_in_expr(
                        &mut arm.body,
                        func_sigs,
                        type_table,
                        module_source,
                    );
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    Self::convert_func_refs_in_expr(p, func_sigs, type_table, module_source);
                }
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                Self::convert_func_refs_in_expr(value, func_sigs, type_table, module_source);
            }
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                Self::convert_func_refs_in_expr(inner, func_sigs, type_table, module_source);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                Self::convert_func_refs_in_expr(scrutinee, func_sigs, type_table, module_source);
                for arm in arms {
                    Self::convert_func_refs_in_block(arm, func_sigs, type_table, module_source);
                }
                Self::convert_func_refs_in_block(default, func_sigs, type_table, module_source);
            }
            // Terminals - nothing to do
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. } // FuncRef not in func_sigs (external) - leave as-is
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!("WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase")
            }
        }
    }

    /// Update `locals` in a function after closure transformation
    fn update_local_types(&self, func: &mut TirFunction) {
        // For each local that stored a closure and was transformed to a struct,
        // update its type from function type to struct type
        for (local_idx, closure_id) in &self.local_to_closure {
            if self.specializable.contains(closure_id)
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
                && let Some(local) = func.locals.get_mut(*local_idx as usize)
            {
                // Functors are reference types
                local.type_id = functor.ref_type_id;
            }
        }
    }

    fn collect_closures_in_block(&mut self, block: &mut TirBlock) {
        for stmt in &mut block.stmts {
            self.collect_closures_in_stmt(stmt);
        }
    }

    fn collect_closures_in_stmt(&mut self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_closures_in_expr(value);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.collect_closures_in_expr(expr);
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
            TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.collect_closures_in_expr(value);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_closures_in_expr(condition);
                self.collect_closures_in_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_closures_in_block(else_blk);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.collect_closures_in_block(body);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_closures_in_expr(scrutinee);
                self.collect_closures_in_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_closures_in_block(else_blk);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.collect_closures_in_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    fn collect_closures_in_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Closure {
                params,
                body,
                captures,
                functor_id,
                ..
            } => {
                // Assign a stable functor_id to this Closure node. Every
                // later pass reads the ID off the node directly.
                let closure_id = self.next_closure_id;
                self.next_closure_id += 1;
                *functor_id = Some(closure_id);

                // Recurse into the body FIRST so nested closures get IDs
                // assigned before we clone the body into `__call`. The
                // clone then carries the same IDs as the original.
                self.collect_closures_in_expr(body);

                let func_type_id = expr.type_id;
                let span = expr.span;
                self.collected_closures.push(CollectedClosure {
                    id: closure_id,
                    params: params.clone(),
                    body: (**body).clone(),
                    captures: captures.clone(),
                    return_type: body.type_id,
                    func_type_id,
                    span,
                });
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_closures_in_expr(left);
                self.collect_closures_in_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                self.collect_closures_in_expr(inner);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.collect_closures_in_expr(&mut arg.expr);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.collect_closures_in_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.collect_closures_in_expr(receiver);
                for arg in args {
                    self.collect_closures_in_expr(&mut arg.expr);
                }
            }
            TirExprKind::Block(block) => {
                self.collect_closures_in_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_closures_in_expr(condition);
                self.collect_closures_in_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.collect_closures_in_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_closures_in_expr(&mut field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_closures_in_expr(elem);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_closures_in_expr(target);
                self.collect_closures_in_expr(value);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_closures_in_expr(array);
                self.collect_closures_in_expr(index);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_closures_in_expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.collect_closures_in_expr(guard);
                    }
                    self.collect_closures_in_expr(&mut arm.body);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_closures_in_expr(callee);
                for arg in args {
                    self.collect_closures_in_expr(arg);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_closures_in_expr(functor);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_closures_in_expr(payload_expr);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_closures_in_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_closures_in_expr(value);
            }
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.collect_closures_in_expr(expr);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_closures_in_expr(scrutinee);
                for arm in arms {
                    self.collect_closures_in_block(arm);
                }
                self.collect_closures_in_block(default);
            }
            // Terminals - no closures
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }

    /// Analyze a block to identify which closures are safe to transform
    fn analyze_closure_safety_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.analyze_closure_safety_stmt(stmt);
        }
    }

    fn analyze_closure_safety_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                // If this local stores a closure, track it. The closure's
                // ID was assigned during the collect pass and lives on the
                // node; reading it here keeps every pass in sync without a
                // separate counter.
                if let TirExprKind::Closure {
                    functor_id: Some(closure_id),
                    ..
                } = &value.kind
                {
                    self.local_to_closure.insert(*local_index, *closure_id);
                    // Initially mark as safe; will be removed if passed as argument
                    self.specializable.insert(*closure_id);
                }
                // Analyze the value expression
                self.analyze_closure_safety_expr(value, false);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.analyze_closure_safety_expr(expr, false);
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
            TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.analyze_closure_safety_expr(value, false);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.analyze_closure_safety_expr(condition, false);
                self.analyze_closure_safety_block(then_block);
                if let Some(else_blk) = else_block {
                    self.analyze_closure_safety_block(else_blk);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.analyze_closure_safety_block(body);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.analyze_closure_safety_expr(scrutinee, false);
                self.analyze_closure_safety_block(then_block);
                if let Some(else_blk) = else_block {
                    self.analyze_closure_safety_block(else_blk);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.analyze_closure_safety_expr(value, false);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    /// Analyze an expression for closure safety
    /// `in_arg_position` is true when this expression is being passed as an argument
    fn analyze_closure_safety_expr(&mut self, expr: &TirExpr, in_arg_position: bool) {
        match &expr.kind {
            TirExprKind::Closure {
                body,
                functor_id: Some(closure_id),
                ..
            } => {
                // If a closure appears directly as an argument, it's not safe to transform
                if in_arg_position {
                    self.specializable.swap_remove(closure_id);
                }

                // The closure body uses a fresh local-index namespace
                // starting at 0, so `local_to_closure` entries from the
                // outer scope must not leak in: a `let inner = ||..`
                // inside this body would otherwise insert at an index
                // that collides with an outer-scope local. Save/restore
                // the map across the descent so each closure scope is
                // isolated.
                let saved = std::mem::take(&mut self.local_to_closure);
                self.analyze_closure_safety_expr(body, false);
                self.local_to_closure = saved;
            }
            TirExprKind::Closure {
                functor_id: None, ..
            } => {
                unreachable!(
                    "Closure node missing functor_id; the collect pass should assign it (span: {:?}, kind: {:?})",
                    expr.span, &expr.kind
                )
            }
            TirExprKind::Local { index, .. } => {
                // If a local that holds a closure is passed as an argument, mark it unsafe
                if in_arg_position && let Some(closure_id) = self.local_to_closure.get(index) {
                    self.specializable.swap_remove(closure_id);
                }
            }
            TirExprKind::Call { args, .. } => {
                // Arguments are in argument position
                for arg in args {
                    self.analyze_closure_safety_expr(&arg.expr, true);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                // Arguments are in argument position
                for arg in args {
                    self.analyze_closure_safety_expr(arg, true);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                // Receiver is not in argument position for method calls
                self.analyze_closure_safety_expr(receiver, false);
                // Arguments are in argument position
                for arg in args {
                    self.analyze_closure_safety_expr(&arg.expr, true);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                // Callee is not an argument (it's what's being called)
                self.analyze_closure_safety_expr(callee, false);
                // Arguments are in argument position
                for arg in args {
                    self.analyze_closure_safety_expr(arg, true);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.analyze_closure_safety_expr(functor, in_arg_position);
            }
            TirExprKind::Binary { left, right, .. } => {
                self.analyze_closure_safety_expr(left, false);
                self.analyze_closure_safety_expr(right, false);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                self.analyze_closure_safety_expr(inner, false);
            }
            TirExprKind::Block(block) => {
                self.analyze_closure_safety_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.analyze_closure_safety_expr(condition, false);
                self.analyze_closure_safety_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.analyze_closure_safety_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.analyze_closure_safety_expr(&field.value, false);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.analyze_closure_safety_expr(elem, false);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.analyze_closure_safety_expr(target, false);
                self.analyze_closure_safety_expr(value, false);
            }
            TirExprKind::Index { expr: array, index } => {
                self.analyze_closure_safety_expr(array, false);
                self.analyze_closure_safety_expr(index, false);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.analyze_closure_safety_expr(scrutinee, false);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.analyze_closure_safety_expr(guard, false);
                    }
                    self.analyze_closure_safety_expr(&arm.body, false);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.analyze_closure_safety_expr(payload_expr, false);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.analyze_closure_safety_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.analyze_closure_safety_expr(value, false);
            }
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.analyze_closure_safety_expr(expr, false);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.analyze_closure_safety_expr(scrutinee, false);
                for arm in arms {
                    self.analyze_closure_safety_block(arm);
                }
                self.analyze_closure_safety_block(default);
            }
            // Terminals
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }

    fn generate_functor_items(&mut self, type_table: &mut TypeTable) {
        // The collect pass pushes nested closures before their parents (it
        // recurses into the body before pushing the outer, so the cloned
        // body carries the nested IDs). Sort by id so `functor_infos[id]`
        // is the functor for the closure with that id — every later pass
        // uses index-by-id lookups.
        //
        // Move out of `self.collected_closures` to avoid cloning the bodies:
        // the loop body needs `&mut self` to push to `generated_structs` and
        // `functor_infos`, but the borrow checker would otherwise see one
        // long borrow over `collected_closures`. The list isn't read again
        // after this pass, so we drop it at the end.
        let mut collected_closures = std::mem::take(&mut self.collected_closures);
        collected_closures.sort_by_key(|c| c.id);
        for collected in &collected_closures {
            // Extract the actual return type from the closure's function type
            // This is more reliable than body.type_id for closures with block bodies
            let return_type = match type_table.get(collected.func_type_id) {
                ResolvedType::Function { return_type, .. } => *return_type,
                _ => collected.return_type, // Fallback to body type
            };

            // Generate struct name and type
            let struct_name = format!("__Closure_{}", collected.id);
            let struct_type_id =
                type_table.make_struct(struct_name.clone(), self.module_source.clone());

            // Generate struct definition with capture fields
            let fields: Vec<TirField> = collected
                .captures
                .iter()
                .enumerate()
                .map(|(i, cap)| TirField {
                    name: format!("__capture_{i}"),
                    is_pub: false,
                    type_id: cap.type_id,
                    index: i as u32,
                    span: collected.span,
                    is_hidden: false,
                    serde_rename: None,
                    serde_default: false,
                    default_expr: None,
                })
                .collect();

            let tir_struct = TirStruct {
                name: struct_name.clone(),
                module_source: self.module_source.clone(),
                is_pub: false,
                type_params: Vec::new(),
                monomorph_info: None,
                fields,
                span: collected.span,
                serde_rename_all: None,
            };
            self.generated_structs.push(tir_struct);

            // Generate __call method
            // Use a qualified name for the function to avoid collisions in the inliner's candidate map
            let simple_method_name = "__call".to_string();
            let qualified_method_name = MethodName::format_local(&struct_name, None, "__call");
            let self_ref_type = type_table.make_ref(struct_type_id);

            // Parameters: self + closure params
            let mut params = Vec::new();
            params.push(TirParam {
                name: "self".to_string(),
                type_id: self_ref_type,
                local_index: 0,
                is_mut: false,
                span: collected.span,
                default_expr: None,
            });

            for (i, (name, type_id)) in collected.params.iter().enumerate() {
                params.push(TirParam {
                    name: name.clone(),
                    type_id: *type_id,
                    local_index: (i + 1) as u32,
                    is_mut: false,
                    span: collected.span,
                    default_expr: None,
                });
            }

            // Transform the body: Capture { index } -> FieldAccess { self, __capture_{index} }
            let transformed_body = self.transform_closure_body(
                &collected.body,
                &collected.captures,
                struct_type_id,
                self_ref_type,
                &collected.span,
            );

            // Handle body wrapping based on body type
            // For block bodies, extract statements directly to preserve Return handling during inlining
            // For expression bodies, wrap in Return
            let body_stmts = match &transformed_body.kind {
                TirExprKind::Block(block) => {
                    // Block body: use statements directly (they already contain Return statements)
                    // This is important for inlining: the inliner's remap_stmt_with_label
                    // converts Return to Break, but only at the statement level, not inside
                    // Block expressions.
                    block.stmts.clone()
                }
                _ => {
                    if return_type == TypeTable::UNIT {
                        // Unit return: just evaluate the expression for side effects
                        vec![TirStmt::new(
                            TirStmtKind::Expr(transformed_body),
                            collected.span,
                        )]
                    } else {
                        // Expression body that returns a value
                        vec![TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(transformed_body),
                            },
                            collected.span,
                        )]
                    }
                }
            };

            let body_block = TirBlock::new(body_stmts, collected.span);

            // Collect locals: self + params + internal locals from body.
            // Parameters are locals 0 (self) through params.len(); the
            // closure-functor's `__call` method receives the env struct as
            // self, so the first slot is the synthesised self ref.
            let param_count = 1 + collected.params.len() as u32;
            let mut locals: Vec<TirLocal> = Vec::with_capacity(param_count as usize);
            locals.push(TirLocal {
                name: "self".to_string(),
                type_id: self_ref_type,
                is_mut: false,
            });
            for (name, ty) in &collected.params {
                locals.push(TirLocal {
                    name: name.clone(),
                    type_id: *ty,
                    is_mut: false,
                });
            }

            // Collect internal locals from the body (Let statements with index >= param_count)
            let mut body_locals: Vec<(u32, TypeId)> = Vec::new();
            Self::collect_locals_from_block(&body_block, &mut body_locals);

            // Extend `locals` with body locals, sorted by index. Body locals
            // come from the closure body's `Let` statements; the source
            // names are recovered later in `wir_build` via
            // `TirFunction::locals[idx].name`, so here we only need stable
            // synthetic placeholders.
            body_locals.sort_by_key(|(idx, _)| *idx);
            for (idx, type_id) in &body_locals {
                // Ensure we only add locals beyond parameter range
                if *idx >= param_count {
                    // Extend vector if needed to accommodate sparse indices
                    while locals.len() <= *idx as usize {
                        let placeholder_idx = locals.len() as u32;
                        locals.push(TirLocal::synth(
                            placeholder_idx,
                            TypeTable::UNKNOWN,
                            false,
                        ));
                    }
                    locals[*idx as usize] = TirLocal::synth(*idx, *type_id, false);
                }
            }

            // local_count is the total number of locals
            let local_count = locals.len() as u32;

            // method_info tells codegen how to register this function with the proper mangled name
            let method_info = LocalMethodName::new(
                struct_name.clone(),        // __Closure_0
                None,                       // no trait
                simple_method_name.clone(), // __call (just the method name)
            );

            let call_method = TirFunction {
                module_source: self.module_source.clone(),
                is_async: false,
                name: qualified_method_name,
                is_pub: false,
                is_export: false, // Closure method, not a world export
                type_params: Vec::new(),
                impl_type_params: Vec::new(),
                monomorph_info: None,
                method_info: Some(method_info),
                params,
                return_type,
                task_return_type: None,
                effects: Vec::new(),
                stores: vec![],
                body: Some(body_block),
                span: collected.span,
                local_count,
                locals,
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
                kind: FunctionKind::Regular,
            };

            let call_method_rc = Rc::new(RefCell::new(call_method));
            self.generated_functions.push(Rc::clone(&call_method_rc));

            self.functor_infos.push(ClosureFunctor {
                module_source: self.module_source.clone(),
                id: collected.id,
                struct_name,
                struct_type_id,
                ref_type_id: self_ref_type,
                call_method: call_method_rc,
                captures: collected.captures.clone(),
            });
        }
    }

    /// Collect all local variable declarations from a block, including nested blocks.
    /// Returns pairs of (`local_index`, `type_id`) for each Let statement found.
    fn collect_locals_from_block(block: &TirBlock, locals: &mut Vec<(u32, TypeId)>) {
        for stmt in &block.stmts {
            Self::collect_locals_from_stmt(stmt, locals);
        }
    }

    fn collect_locals_from_stmt(stmt: &TirStmt, locals: &mut Vec<(u32, TypeId)>) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                type_id,
                value,
                ..
            } => {
                locals.push((*local_index, *type_id));
                Self::collect_locals_from_expr(value, locals);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                Self::collect_locals_from_expr(expr, locals);
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
            TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    Self::collect_locals_from_expr(value, locals);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::collect_locals_from_expr(condition, locals);
                Self::collect_locals_from_block(then_block, locals);
                if let Some(else_blk) = else_block {
                    Self::collect_locals_from_block(else_blk, locals);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                Self::collect_locals_from_block(body, locals);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                Self::collect_locals_from_expr(scrutinee, locals);
                Self::collect_locals_from_block(then_block, locals);
                if let Some(else_blk) = else_block {
                    Self::collect_locals_from_block(else_blk, locals);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                Self::collect_locals_from_expr(value, locals);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    fn collect_locals_from_expr(expr: &TirExpr, locals: &mut Vec<(u32, TypeId)>) {
        match &expr.kind {
            TirExprKind::Block(block) => {
                Self::collect_locals_from_block(block, locals);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::collect_locals_from_expr(condition, locals);
                Self::collect_locals_from_block(then_branch, locals);
                if let Some(else_blk) = else_branch {
                    Self::collect_locals_from_block(else_blk, locals);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::collect_locals_from_expr(left, locals);
                Self::collect_locals_from_expr(right, locals);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                Self::collect_locals_from_expr(inner, locals);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::collect_locals_from_expr(&arg.expr, locals);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    Self::collect_locals_from_expr(arg, locals);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::collect_locals_from_expr(receiver, locals);
                for arg in args {
                    Self::collect_locals_from_expr(&arg.expr, locals);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::collect_locals_from_expr(callee, locals);
                for arg in args {
                    Self::collect_locals_from_expr(arg, locals);
                }
            }
            TirExprKind::Index { expr: array, index } => {
                Self::collect_locals_from_expr(array, locals);
                Self::collect_locals_from_expr(index, locals);
            }
            TirExprKind::Assign { target, value } => {
                Self::collect_locals_from_expr(target, locals);
                Self::collect_locals_from_expr(value, locals);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::collect_locals_from_expr(&field.value, locals);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::collect_locals_from_expr(elem, locals);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_locals_from_expr(scrutinee, locals);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        Self::collect_locals_from_expr(guard, locals);
                    }
                    Self::collect_locals_from_expr(&arm.body, locals);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    Self::collect_locals_from_expr(payload_expr, locals);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                Self::collect_locals_from_block(block, locals);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                Self::collect_locals_from_expr(scrutinee, locals);
                for arm in arms {
                    Self::collect_locals_from_block(arm, locals);
                }
                Self::collect_locals_from_block(default, locals);
            }
            TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                Self::collect_locals_from_expr(inner, locals);
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                Self::collect_locals_from_expr(functor, locals);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                Self::collect_locals_from_expr(value, locals);
            }
            // Terminals - no locals to collect
            _ => {}
        }
    }

    /// Transform closure body: replace Capture with `FieldAccess` on self
    fn transform_closure_body(
        &self,
        expr: &TirExpr,
        captures: &[TirCapture],
        struct_type_id: TypeId,
        self_ref_type: TypeId,
        span: &Span,
    ) -> TirExpr {
        match &expr.kind {
            TirExprKind::Capture { index, name: _ } => {
                // Transform: Capture { index } -> self.__capture_{index}
                let self_expr = TirExpr::new(
                    TirExprKind::Local {
                        index: 0, // self is at local 0
                        name: "self".to_string(),
                    },
                    self_ref_type,
                    *span,
                );

                let field_name = format!("__capture_{index}");
                let cap_type = captures
                    .get(*index as usize)
                    .map_or(TypeTable::UNKNOWN, |c| c.type_id);

                TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(self_expr),
                        field_index: *index,
                        field_name,
                    },
                    cap_type,
                    *span,
                )
            }
            TirExprKind::Binary { left, op, right } => TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(self.transform_closure_body(
                        left,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    op: *op,
                    right: Box::new(self.transform_closure_body(
                        right,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Unary { op, expr: inner } => TirExpr::new(
                TirExprKind::Unary {
                    op: *op,
                    expr: Box::new(self.transform_closure_body(
                        inner,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Local { index, name } => {
                // Adjust local indices: closure params start at local 1 (after self)
                // Original closure param 0 becomes local 1, etc.
                TirExpr::new(
                    TirExprKind::Local {
                        index: index + 1,
                        name: name.clone(),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Block(block) => {
                let transformed_block = self.transform_closure_body_block(
                    block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                );
                TirExpr::new(
                    TirExprKind::Block(transformed_block),
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => {
                let transformed_block = self.transform_closure_body_block(
                    block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                );
                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: label.clone(),
                        block: transformed_block,
                        result_type: *result_type,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(self.transform_closure_body(
                        condition,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    then_branch: self.transform_closure_body_block(
                        then_branch,
                        captures,
                        struct_type_id,
                        self_ref_type,
                    ),
                    else_branch: else_branch.as_ref().map(|b| {
                        self.transform_closure_body_block(
                            b,
                            captures,
                            struct_type_id,
                            self_ref_type,
                        )
                    }),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(self.transform_closure_body(
                        inner,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    target_type: *target_type,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                let new_fields = fields
                    .iter()
                    .map(|f| TirStructField {
                        name: f.name.clone(),
                        value: self.transform_closure_body(
                            &f.value,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        ),
                        field_index: f.field_index,
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::StructLiteral {
                        struct_type: *struct_type,
                        struct_name: struct_name.clone(),
                        fields: new_fields,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Call {
                func,
                type_args,
                args,
            } => {
                let new_args = args
                    .iter()
                    .map(|a| {
                        CallArg::new(
                            self.transform_closure_body(
                                &a.expr,
                                captures,
                                struct_type_id,
                                self_ref_type,
                                span,
                            ),
                            a.is_mut,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::Call {
                        func: func.clone(),
                        type_args: type_args.clone(),
                        args: new_args,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => {
                let new_receiver = self.transform_closure_body(
                    receiver,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_args = args
                    .iter()
                    .map(|a| {
                        CallArg::new(
                            self.transform_closure_body(
                                &a.expr,
                                captures,
                                struct_type_id,
                                self_ref_type,
                                span,
                            ),
                            a.is_mut,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::method_call(
                        Box::new(new_receiver),
                        func.clone(),
                        type_args.clone(),
                        new_args,
                    ),
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                field_name,
            } => {
                let new_inner = self.transform_closure_body(
                    inner,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(new_inner),
                        field_index: *field_index,
                        field_name: field_name.clone(),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Assign { target, value } => {
                let new_target = self.transform_closure_body(
                    target,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_value = self.transform_closure_body(
                    value,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::Assign {
                        target: Box::new(new_target),
                        value: Box::new(new_value),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Index { expr: array, index } => {
                let new_array = self.transform_closure_body(
                    array,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_index = self.transform_closure_body(
                    index,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::Index {
                        expr: Box::new(new_array),
                        index: Box::new(new_index),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::TupleLiteral { elements } => {
                let new_elements = elements
                    .iter()
                    .map(|e| {
                        self.transform_closure_body(
                            e,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::TupleLiteral {
                        elements: new_elements,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::TupleSpread { expr: inner } => {
                let new_inner = self.transform_closure_body(
                    inner,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::TupleSpread {
                        expr: Box::new(new_inner),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::TupleZip { expr: inner } => {
                let new_inner = self.transform_closure_body(
                    inner,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::TupleZip {
                        expr: Box::new(new_inner),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::TypePackExpansion {
                call_expr: inner,
                pack_type_id,
            } => {
                let new_inner = self.transform_closure_body(
                    inner,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                TirExpr::new(
                    TirExprKind::TypePackExpansion {
                        call_expr: Box::new(new_inner),
                        pack_type_id: *pack_type_id,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::CmRawCall { local_name, args } => {
                let new_args = args
                    .iter()
                    .map(|a| {
                        self.transform_closure_body(
                            a,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::CmRawCall {
                        local_name: local_name.clone(),
                        args: new_args,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::IndirectCall { callee, args } => {
                let new_callee = self.transform_closure_body(
                    callee,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_args = args
                    .iter()
                    .map(|a| {
                        self.transform_closure_body(
                            a,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        )
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::IndirectCall {
                        callee: Box::new(new_callee),
                        args: new_args,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => TirExpr::new(
                TirExprKind::ClosureToCanonical {
                    functor: Box::new(self.transform_closure_body(
                        functor,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    functor_id: *functor_id,
                    target_fn_type: *target_fn_type,
                    closure_module: closure_module.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let new_scrutinee = self.transform_closure_body(
                    scrutinee,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_arms = arms
                    .iter()
                    .map(|arm| TirMatchArm {
                        pattern: self.transform_closure_body_pattern(&arm.pattern),
                        guard: arm.guard.as_ref().map(|g| {
                            self.transform_closure_body(
                                g,
                                captures,
                                struct_type_id,
                                self_ref_type,
                                span,
                            )
                        }),
                        body: self.transform_closure_body(
                            &arm.body,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        ),
                        span: arm.span,
                    })
                    .collect();
                TirExpr::new(
                    TirExprKind::Match {
                        expr: Box::new(new_scrutinee),
                        arms: new_arms,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => {
                let new_scrutinee = self.transform_closure_body(
                    scrutinee,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                );
                let new_arms = arms
                    .iter()
                    .map(|arm| {
                        self.transform_closure_body_block(
                            arm,
                            captures,
                            struct_type_id,
                            self_ref_type,
                        )
                    })
                    .collect();
                let new_default = self.transform_closure_body_block(
                    default,
                    captures,
                    struct_type_id,
                    self_ref_type,
                );
                TirExpr::new(
                    TirExprKind::Switch {
                        scrutinee: Box::new(new_scrutinee),
                        min_value: *min_value,
                        arms: new_arms,
                        default: new_default,
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: *variant_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                    payload: payload.as_ref().map(|p| {
                        Box::new(self.transform_closure_body(
                            p,
                            captures,
                            struct_type_id,
                            self_ref_type,
                            span,
                        ))
                    }),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantTag { expr: inner } => TirExpr::new(
                TirExprKind::VariantTag {
                    expr: Box::new(self.transform_closure_body(
                        inner,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantTest {
                expr: inner,
                case_index,
                case_name,
            } => TirExpr::new(
                TirExprKind::VariantTest {
                    expr: Box::new(self.transform_closure_body(
                        inner,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    case_index: *case_index,
                    case_name: case_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantPayload {
                expr: inner,
                case_index,
                payload_type,
            } => TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(self.transform_closure_body(
                        inner,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => TirExpr::new(
                TirExprKind::GlobalVarSet {
                    module_source: module_source.clone(),
                    name: name.clone(),
                    value: Box::new(self.transform_closure_body(
                        value,
                        captures,
                        struct_type_id,
                        self_ref_type,
                        span,
                    )),
                },
                expr.type_id,
                expr.span,
            ),
            // Terminals that don't need transformation
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::EnumConstruct { .. } => expr.clone(),
            // Nested closures are a separate scope: their body uses its
            // own local-index namespace and gets its own `__call` method
            // generated in `generate_functor_items`. Don't recurse into
            // the body or shift its locals here. Cloning preserves the
            // `Closure` expression so the third pass
            // (`transform_block`) can later rewrite it into a struct
            // literal or `ClosureToCanonical`.
            //
            // Note: the nested closure's `captures[*].outer_index`
            // references locals in *this* closure's pre-shift scope,
            // not in the synthesized `__call`. Because nested-closure
            // construction sites are themselves rewritten by the third
            // pass to read capture values via the shifted local map,
            // the outer-index field is treated as a logical reference
            // to the source-level local rather than a raw shifted
            // index — so we leave it alone here.
            TirExprKind::Closure { .. } => expr.clone(),
            // Constructs that are desugared / lowered before this pass.
            // Reaching them here would silently drop sub-expressions (and any
            // captured locals inside them), so make the failure explicit.
            TirExprKind::TemplateString { .. }
            | TirExprKind::WithHandler { .. }
            | TirExprKind::Resume { .. } => {
                unreachable!(
                    "[lower/closure] unexpected expr kind in closure body during transform_closure_body at {:?}: {:?} — should be lowered earlier",
                    expr.span, &expr.kind
                )
            }
        }
    }

    /// Transform statements within a closure body block
    fn transform_closure_body_block(
        &self,
        block: &TirBlock,
        captures: &[TirCapture],
        struct_type_id: TypeId,
        self_ref_type: TypeId,
    ) -> TirBlock {
        let stmts: Vec<TirStmt> = block
            .stmts
            .iter()
            .map(|stmt| {
                self.transform_closure_body_stmt(stmt, captures, struct_type_id, self_ref_type)
            })
            .collect();
        TirBlock::new(stmts, block.span)
    }

    /// Transform a statement within a closure body
    fn transform_closure_body_stmt(
        &self,
        stmt: &TirStmt,
        captures: &[TirCapture],
        struct_type_id: TypeId,
        self_ref_type: TypeId,
    ) -> TirStmt {
        let span = &stmt.span;
        let kind = match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                name,
                is_mut,
                is_reactive,
                type_id,
                value,
                ..
            } => TirStmtKind::Let {
                local_index: local_index + 1, // Shift by 1 for self parameter
                name: name.clone(),
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.transform_closure_body(
                    value,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
                skip_value_copy: false,
            },
            TirStmtKind::Expr(expr) => TirStmtKind::Expr(self.transform_closure_body(
                expr,
                captures,
                struct_type_id,
                self_ref_type,
                span,
            )),
            TirStmtKind::Return { value } => TirStmtKind::Return {
                value: value.as_ref().map(|v| {
                    self.transform_closure_body(v, captures, struct_type_id, self_ref_type, span)
                }),
            },
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => TirStmtKind::If {
                condition: self.transform_closure_body(
                    condition,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
                then_block: self.transform_closure_body_block(
                    then_block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
                else_block: else_block.as_ref().map(|b| {
                    self.transform_closure_body_block(b, captures, struct_type_id, self_ref_type)
                }),
            },
            TirStmtKind::Loop { body } => TirStmtKind::Loop {
                body: self.transform_closure_body_block(
                    body,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
            },
            TirStmtKind::LabeledBlock { label, block } => TirStmtKind::LabeledBlock {
                label: label.clone(),
                block: self.transform_closure_body_block(
                    block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
            },
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => TirStmtKind::IfLet {
                scrutinee: self.transform_closure_body(
                    scrutinee,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
                pattern: self.transform_closure_body_pattern(pattern),
                then_block: self.transform_closure_body_block(
                    then_block,
                    captures,
                    struct_type_id,
                    self_ref_type,
                ),
                else_block: else_block.as_ref().map(|b| {
                    self.transform_closure_body_block(b, captures, struct_type_id, self_ref_type)
                }),
            },
            TirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => TirStmtKind::LetDestructure {
                pattern: self.transform_closure_body_pattern(pattern),
                is_mut: *is_mut,
                value: self.transform_closure_body(
                    value,
                    captures,
                    struct_type_id,
                    self_ref_type,
                    span,
                ),
            },
            TirStmtKind::Break { label, value } => TirStmtKind::Break {
                label: label.clone(),
                value: value.as_ref().map(|v| {
                    self.transform_closure_body(v, captures, struct_type_id, self_ref_type, span)
                }),
            },
            TirStmtKind::Continue => TirStmtKind::Continue,
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        };
        TirStmt::new(kind, stmt.span)
    }

    /// Transform a pattern within a closure body, adjusting local indices by 1 for self parameter
    fn transform_closure_body_pattern(&self, pattern: &TirPattern) -> TirPattern {
        match pattern {
            TirPattern::Wildcard => TirPattern::Wildcard,
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => TirPattern::Binding {
                name: name.clone(),
                local_index: local_index + 1, // Shift by 1 for self parameter
                type_id: *type_id,
            },
            TirPattern::Literal(lit) => TirPattern::Literal(lit.clone()),
            TirPattern::Tuple(patterns, has_rest) => TirPattern::Tuple(
                patterns
                    .iter()
                    .map(|p| self.transform_closure_body_pattern(p))
                    .collect(),
                *has_rest,
            ),
            TirPattern::Variant {
                enum_type,
                variant_name,
                bindings,
                payload_type,
            } => TirPattern::Variant {
                enum_type: *enum_type,
                variant_name: variant_name.clone(),
                bindings: bindings
                    .iter()
                    .map(|p| self.transform_closure_body_pattern(p))
                    .collect(),
                payload_type: *payload_type,
            },
            TirPattern::Enum {
                enum_type,
                case_name,
                case_index,
            } => TirPattern::Enum {
                enum_type: *enum_type,
                case_name: case_name.clone(),
                case_index: *case_index,
            },
            TirPattern::Struct {
                struct_type,
                fields,
                has_rest,
            } => TirPattern::Struct {
                struct_type: *struct_type,
                fields: fields
                    .iter()
                    .map(|f| TirStructPatternField {
                        field_name: f.field_name.clone(),
                        field_index: f.field_index,
                        pattern: self.transform_closure_body_pattern(&f.pattern),
                    })
                    .collect(),
                has_rest: *has_rest,
            },
            TirPattern::Or(alternatives) => TirPattern::Or(
                alternatives
                    .iter()
                    .map(|p| self.transform_closure_body_pattern(p))
                    .collect(),
            ),
            TirPattern::ConstantValue { expr } => TirPattern::ConstantValue { expr: expr.clone() },
            TirPattern::Range {
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

    /// Generate specialized functions for calls with closure arguments to fn-type parameters.
    ///
    /// This implements WEP Phase 3: when a function takes `fn(A) -> B` and is called with
    /// a closure, we generate a specialized version where:
    /// 1. The fn-type parameter becomes the functor struct type
    /// 2. `IndirectCall` on that parameter becomes `MethodCall` on __call
    fn generate_fn_param_specializations(
        &mut self,
        func_refs: &[Rc<RefCell<TirFunction>>],
        impls: &[TirImpl],
        type_table: &mut TypeTable,
    ) {
        // Build a map from function name to function for quick lookup
        let mut func_by_name: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
        for func_rc in func_refs {
            let func = func_rc.borrow();
            func_by_name.insert(func.name.clone(), Rc::clone(func_rc));
        }
        for impl_block in impls {
            for method in &impl_block.methods {
                let name = method.name.clone();
                // We can't get an Rc from a TirFunction reference directly
                // For impl methods, we'll handle them separately
                // For now, just process top-level functions
                drop(name);
            }
        }

        // Collect specialization requests by scanning all function bodies.
        // Each Closure node already carries its `functor_id` from the
        // collect pass, so we don't need a separate counter.
        let mut spec_requests: Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)> = Vec::new();

        for func_rc in func_refs {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.collect_fn_param_specs(body, &func_by_name, type_table, &mut spec_requests);
            }
        }

        // Also scan impl methods
        for impl_block in impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.collect_fn_param_specs(
                        body,
                        &func_by_name,
                        type_table,
                        &mut spec_requests,
                    );
                }
            }
        }

        // Generate specialized functions for each unique key
        for (key, callee_rc) in spec_requests {
            if self.fn_param_specializations.contains_key(&key) {
                continue; // Already generated
            }

            // Skip specialization if any fn-param is stored in a struct field
            // This would cause type mismatches since struct fields expect fn(...) not &__Closure_N
            let callee = callee_rc.borrow();
            // Check if this is an instance method (has self parameter)
            // Note: static methods have method_info but no self parameter
            let has_self_param = callee.params.first().is_some_and(|p| p.name == "self");
            let param_offset = u32::from(has_self_param);
            let fn_param_indices: Vec<u32> = key
                .functor_types
                .iter()
                .map(|(arg_idx, _)| arg_idx + param_offset)
                .collect();

            if let Some(body) = &callee.body
                && self.fn_param_stored_in_struct_field(body, &fn_param_indices)
            {
                // Skip this specialization - the closure is stored in a struct field
                continue;
            }
            drop(callee);

            let specialized_name = self.generate_specialized_function(&key, &callee_rc, type_table);
            self.fn_param_specializations.insert(key, specialized_name);
        }
    }

    /// Check if any of the given local indices (fn-params) are used as struct field values.
    /// If so, we can't specialize because the struct field expects fn(...) not &__`Closure_N`.
    fn fn_param_stored_in_struct_field(&self, block: &TirBlock, fn_param_indices: &[u32]) -> bool {
        for stmt in &block.stmts {
            if self.fn_param_in_struct_field_stmt(stmt, fn_param_indices) {
                return true;
            }
        }
        false
    }

    fn fn_param_in_struct_field_stmt(&self, stmt: &TirStmt, fn_param_indices: &[u32]) -> bool {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.fn_param_in_struct_field_expr(expr, fn_param_indices)
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => false,
            TirStmtKind::Break { value, .. } => value
                .as_ref()
                .is_some_and(|v| self.fn_param_in_struct_field_expr(v, fn_param_indices)),
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.fn_param_in_struct_field_expr(condition, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(then_block, fn_param_indices)
                    || else_block
                        .as_ref()
                        .is_some_and(|b| self.fn_param_stored_in_struct_field(b, fn_param_indices))
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.fn_param_stored_in_struct_field(body, fn_param_indices)
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(then_block, fn_param_indices)
                    || else_block
                        .as_ref()
                        .is_some_and(|b| self.fn_param_stored_in_struct_field(b, fn_param_indices))
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    fn fn_param_in_struct_field_expr(&self, expr: &TirExpr, fn_param_indices: &[u32]) -> bool {
        match &expr.kind {
            // Key case: check if fn-param local is used as struct field value
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    // Check if field value is a Local that is one of our fn-params
                    if let TirExprKind::Local { index, .. } = &field.value.kind
                        && fn_param_indices.contains(index)
                    {
                        return true;
                    }
                    // Also recurse into nested expressions
                    if self.fn_param_in_struct_field_expr(&field.value, fn_param_indices) {
                        return true;
                    }
                }
                false
            }
            // Recurse into sub-expressions
            TirExprKind::Binary { left, right, .. } => {
                self.fn_param_in_struct_field_expr(left, fn_param_indices)
                    || self.fn_param_in_struct_field_expr(right, fn_param_indices)
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => self.fn_param_in_struct_field_expr(inner, fn_param_indices),
            TirExprKind::Call { args, .. } => args
                .iter()
                .any(|a| self.fn_param_in_struct_field_expr(&a.expr, fn_param_indices)),
            TirExprKind::CmRawCall { args, .. } => args
                .iter()
                .any(|a| self.fn_param_in_struct_field_expr(a, fn_param_indices)),
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.fn_param_in_struct_field_expr(receiver, fn_param_indices)
                    || args
                        .iter()
                        .any(|a| self.fn_param_in_struct_field_expr(&a.expr, fn_param_indices))
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.fn_param_in_struct_field_expr(callee, fn_param_indices)
                    || args
                        .iter()
                        .any(|a| self.fn_param_in_struct_field_expr(a, fn_param_indices))
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.fn_param_in_struct_field_expr(functor, fn_param_indices)
            }
            TirExprKind::Block(block) => {
                self.fn_param_stored_in_struct_field(block, fn_param_indices)
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.fn_param_in_struct_field_expr(condition, fn_param_indices)
                    || self.fn_param_stored_in_struct_field(then_branch, fn_param_indices)
                    || else_branch
                        .as_ref()
                        .is_some_and(|b| self.fn_param_stored_in_struct_field(b, fn_param_indices))
            }
            TirExprKind::TupleLiteral { elements } => elements
                .iter()
                .any(|e| self.fn_param_in_struct_field_expr(e, fn_param_indices)),
            TirExprKind::Assign { target, value } => {
                self.fn_param_in_struct_field_expr(target, fn_param_indices)
                    || self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            TirExprKind::Index { expr: array, index } => {
                self.fn_param_in_struct_field_expr(array, fn_param_indices)
                    || self.fn_param_in_struct_field_expr(index, fn_param_indices)
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || arms.iter().any(|arm| {
                        arm.guard.as_ref().is_some_and(|g| {
                            self.fn_param_in_struct_field_expr(g, fn_param_indices)
                        }) || self.fn_param_in_struct_field_expr(&arm.body, fn_param_indices)
                    })
            }
            TirExprKind::VariantConstruct { payload, .. } => payload
                .as_ref()
                .is_some_and(|p| self.fn_param_in_struct_field_expr(p, fn_param_indices)),
            TirExprKind::LabeledBlock { block, .. } => {
                self.fn_param_stored_in_struct_field(block, fn_param_indices)
            }
            TirExprKind::Closure { body, .. } => {
                self.fn_param_in_struct_field_expr(body, fn_param_indices)
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.fn_param_in_struct_field_expr(value, fn_param_indices)
            }
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.fn_param_in_struct_field_expr(expr, fn_param_indices)
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.fn_param_in_struct_field_expr(scrutinee, fn_param_indices)
                    || arms
                        .iter()
                        .any(|arm| self.fn_param_stored_in_struct_field(arm, fn_param_indices))
                    || self.fn_param_stored_in_struct_field(default, fn_param_indices)
            }
            // Terminals
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => false,
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }

    /// Collect fn-param specialization requests from a block
    fn collect_fn_param_specs(
        &mut self,
        block: &TirBlock,
        func_by_name: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
        requests: &mut Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)>,
    ) {
        for stmt in &block.stmts {
            self.collect_fn_param_specs_stmt(stmt, func_by_name, type_table, requests);
        }
    }

    fn collect_fn_param_specs_stmt(
        &mut self,
        stmt: &TirStmt,
        func_by_name: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
        requests: &mut Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)>,
    ) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.collect_fn_param_specs_expr(expr, func_by_name, type_table, requests);
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
            TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_fn_param_specs_expr(condition, func_by_name, type_table, requests);
                self.collect_fn_param_specs(then_block, func_by_name, type_table, requests);
                if let Some(else_blk) = else_block {
                    self.collect_fn_param_specs(else_blk, func_by_name, type_table, requests);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.collect_fn_param_specs(body, func_by_name, type_table, requests);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                self.collect_fn_param_specs(then_block, func_by_name, type_table, requests);
                if let Some(else_blk) = else_block {
                    self.collect_fn_param_specs(else_blk, func_by_name, type_table, requests);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    fn collect_fn_param_specs_expr(
        &mut self,
        expr: &TirExpr,
        func_by_name: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
        requests: &mut Vec<(FnParamSpecKey, Rc<RefCell<TirFunction>>)>,
    ) {
        match &expr.kind {
            TirExprKind::Closure { body, .. } => {
                // Recursively scan the body. The Closure's own functor_id
                // is read directly off the node by `create_fn_param_spec_key`
                // when it appears in argument position.
                self.collect_fn_param_specs_expr(body, func_by_name, type_table, requests);
            }
            // Check for calls with closure arguments
            TirExprKind::Call {
                func,
                args,
                type_args: _,
                ..
            } => {
                // Check if this is a call to a known function with closure args
                if let Some(callee_rc) = func_by_name.get(&func.name.clone()) {
                    let callee = callee_rc.borrow();
                    if let Some(key) = self.create_fn_param_spec_key(
                        &callee.name,
                        &callee.params,
                        args,
                        type_table,
                    ) {
                        requests.push((key, Rc::clone(callee_rc)));
                    }
                }
                // Recursively scan args (this will increment counter for any closure args)
                for arg in args {
                    self.collect_fn_param_specs_expr(&arg.expr, func_by_name, type_table, requests);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                type_args: _,
                ..
            } => {
                // Check if this is a method call with closure args
                if let Some(callee_rc) = func_by_name.get(&func.name.clone()) {
                    let callee = callee_rc.borrow();
                    // Skip self parameter (first param)
                    let params_without_self: Vec<_> =
                        callee.params.iter().skip(1).cloned().collect();
                    if let Some(key) = self.create_fn_param_spec_key(
                        &callee.name,
                        &params_without_self,
                        args,
                        type_table,
                    ) {
                        requests.push((key, Rc::clone(callee_rc)));
                    }
                }
                // Recursively scan receiver first (important for counter sync)
                self.collect_fn_param_specs_expr(receiver, func_by_name, type_table, requests);
                // Then scan args (this will increment counter for any closure args)
                for arg in args {
                    self.collect_fn_param_specs_expr(&arg.expr, func_by_name, type_table, requests);
                }
            }
            // Recurse into sub-expressions
            TirExprKind::Binary { left, right, .. } => {
                self.collect_fn_param_specs_expr(left, func_by_name, type_table, requests);
                self.collect_fn_param_specs_expr(right, func_by_name, type_table, requests);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                self.collect_fn_param_specs_expr(inner, func_by_name, type_table, requests);
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.collect_fn_param_specs_expr(arg, func_by_name, type_table, requests);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_fn_param_specs_expr(callee, func_by_name, type_table, requests);
                for arg in args {
                    self.collect_fn_param_specs_expr(arg, func_by_name, type_table, requests);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_fn_param_specs_expr(functor, func_by_name, type_table, requests);
            }
            TirExprKind::Block(block) => {
                self.collect_fn_param_specs(block, func_by_name, type_table, requests);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_fn_param_specs_expr(condition, func_by_name, type_table, requests);
                self.collect_fn_param_specs(then_branch, func_by_name, type_table, requests);
                if let Some(else_blk) = else_branch {
                    self.collect_fn_param_specs(else_blk, func_by_name, type_table, requests);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_fn_param_specs_expr(
                        &field.value,
                        func_by_name,
                        type_table,
                        requests,
                    );
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_fn_param_specs_expr(elem, func_by_name, type_table, requests);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_fn_param_specs_expr(target, func_by_name, type_table, requests);
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_fn_param_specs_expr(array, func_by_name, type_table, requests);
                self.collect_fn_param_specs_expr(index, func_by_name, type_table, requests);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.collect_fn_param_specs_expr(guard, func_by_name, type_table, requests);
                    }
                    self.collect_fn_param_specs_expr(&arm.body, func_by_name, type_table, requests);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_fn_param_specs_expr(
                        payload_expr,
                        func_by_name,
                        type_table,
                        requests,
                    );
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_fn_param_specs(block, func_by_name, type_table, requests);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_fn_param_specs_expr(value, func_by_name, type_table, requests);
            }
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.collect_fn_param_specs_expr(expr, func_by_name, type_table, requests);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_fn_param_specs_expr(scrutinee, func_by_name, type_table, requests);
                for arm in arms {
                    self.collect_fn_param_specs(arm, func_by_name, type_table, requests);
                }
                self.collect_fn_param_specs(default, func_by_name, type_table, requests);
            }
            // Terminals
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }

    /// Create a fn-param specialization key if there are closure arguments to fn-type parameters.
    /// Reads each closure's `functor_id` directly off its `Closure` node — the
    /// IDs were assigned by the collect pass.
    fn create_fn_param_spec_key(
        &mut self,
        callee_name: &str,
        params: &[TirParam],
        args: &[CallArg],
        type_table: &TypeTable,
    ) -> Option<FnParamSpecKey> {
        let mut functor_types = Vec::new();

        for (i, (param, arg)) in params.iter().zip(args.iter()).enumerate() {
            // Check if param is a function type and arg is a direct closure
            if let ResolvedType::Function { .. } = type_table.get(param.type_id)
                && let TirExprKind::Closure {
                    functor_id: Some(closure_id),
                    ..
                } = &arg.expr.kind
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
            {
                functor_types.push((i as u32, functor.struct_type_id));
            }
        }

        if functor_types.is_empty() {
            return None;
        }

        Some(FnParamSpecKey {
            callee_name: callee_name.to_string(),
            functor_types,
        })
    }

    /// Generate a specialized function where fn-type params become functor struct types
    fn generate_specialized_function(
        &mut self,
        key: &FnParamSpecKey,
        callee_rc: &Rc<RefCell<TirFunction>>,
        type_table: &mut TypeTable,
    ) -> String {
        let callee = callee_rc.borrow();

        // Build specialized name: callee$__Closure_0$__Closure_1...
        let functor_suffix: String =
            key.functor_types
                .iter()
                .fold(String::new(), |mut acc, (_, tid)| {
                    let name = type_table.type_name(*tid);
                    let _ = write!(acc, "${name}");
                    acc
                });
        let specialized_name = format!("{}{}", callee.name, functor_suffix);

        // Build map from argument index to functor type
        // Note: key.functor_types contains argument indices (0 = first arg after receiver for methods)
        let arg_to_functor: IndexMap<u32, TypeId> = key.functor_types.iter().copied().collect();

        // Determine if this is an instance method (has self parameter)
        // Note: static methods have method_info but no self parameter
        let has_self_param = callee.params.first().is_some_and(|p| p.name == "self");
        let param_offset = u32::from(has_self_param);

        // Clone and modify params
        // For methods: params[0] is self, so argument index i maps to params[i + 1]
        let mut new_params = callee.params.clone();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let param_idx = (*arg_idx + param_offset) as usize;
            if param_idx < new_params.len() {
                new_params[param_idx].type_id = type_table.make_ref(functor_type);
            }
        }

        // Clone and modify locals (same indexing as params)
        let mut new_locals = callee.locals.clone();
        for (arg_idx, &functor_type) in &arg_to_functor {
            let local_idx = (*arg_idx + param_offset) as usize;
            if local_idx < new_locals.len() {
                new_locals[local_idx].type_id = type_table.make_ref(functor_type);
            }
        }

        // Build a map from param/local index to functor type for body transformation
        // Inside the function body, locals are referenced by param index
        let local_to_functor: IndexMap<u32, TypeId> = arg_to_functor
            .iter()
            .map(|(arg_idx, functor_type)| (arg_idx + param_offset, *functor_type))
            .collect();

        // Clone body and transform IndirectCall to MethodCall for fn-param locals
        let new_body = callee
            .body
            .as_ref()
            .map(|body| self.specialize_function_body(body, &local_to_functor, type_table));

        // Build specialized method name: method<TypeArgs>$__Closure_0
        // The functor suffix goes AFTER type args, so we use full_method_name()
        // and then clear method_type_args to avoid duplication
        let specialized_method_name = if let Some(ref info) = callee.method_info {
            format!("{}{}", info.full_method_name(), functor_suffix)
        } else {
            // Should not happen for method calls
            format!("{}{}", callee.name, functor_suffix)
        };

        // Update method_info with the specialized method name
        // Note: method_type_args is empty because they're now part of method_name
        let specialized_method_info = callee.method_info.as_ref().map(|info| {
            LocalMethodName {
                struct_name: info.struct_name.clone(),
                base_struct_name: info.base_struct_name.clone(),
                trait_name: info.trait_name.clone(),
                base_trait_name: info.base_trait_name.clone(),
                trait_type_args: info.trait_type_args.clone(),
                method_name: specialized_method_name.clone(),
                method_type_args: Vec::new(), // Type args are now in method_name
                is_type_param_receiver: info.is_type_param_receiver,
                is_ref_impl: false,
                cm_name: info.cm_name.clone(),
            }
        });

        let specialized_func = TirFunction {
            module_source: self.module_source.clone(),
            is_async: false,
            name: specialized_name.clone(),
            is_pub: false,    // Specialized functions are always private
            is_export: false, // Specialized functions are not world exports
            type_params: callee.type_params.clone(),
            impl_type_params: callee.impl_type_params.clone(),
            monomorph_info: callee.monomorph_info.clone(),
            method_info: specialized_method_info,
            params: new_params,
            return_type: callee.return_type,
            task_return_type: None,
            effects: callee.effects.clone(),
            stores: callee.stores.clone(),
            body: new_body,
            span: callee.span,
            local_count: callee.local_count,
            locals: new_locals,
            address_taken_locals: callee.address_taken_locals.clone(),
            stores_aliased_locals: callee.stores_aliased_locals.clone(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: callee.inline_hint,
            comp_features: callee.comp_features,
            export_name: callee.export_name.clone(),
            allocator_tag: callee.allocator_tag.clone(),
            kind: FunctionKind::Regular,
        };

        self.generated_functions
            .push(Rc::new(RefCell::new(specialized_func)));
        specialized_name
    }

    /// Specialize a function body by transforming `IndirectCall` to `MethodCall` for fn-param locals
    fn specialize_function_body(
        &self,
        block: &TirBlock,
        param_to_functor: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TirBlock {
        TirBlock::new(
            block
                .stmts
                .iter()
                .map(|stmt| self.specialize_stmt(stmt, param_to_functor, type_table))
                .collect(),
            block.span,
        )
    }

    fn specialize_stmt(
        &self,
        stmt: &TirStmt,
        param_to_functor: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TirStmt {
        let kind = match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
                ..
            } => TirStmtKind::Let {
                name: name.clone(),
                local_index: *local_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: self.specialize_expr(value, param_to_functor, type_table),
                skip_value_copy: false,
            },
            TirStmtKind::Expr(expr) => {
                TirStmtKind::Expr(self.specialize_expr(expr, param_to_functor, type_table))
            }
            TirStmtKind::Return { value } => TirStmtKind::Return {
                value: value
                    .as_ref()
                    .map(|e| self.specialize_expr(e, param_to_functor, type_table)),
            },
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => TirStmtKind::If {
                condition: self.specialize_expr(condition, param_to_functor, type_table),
                then_block: self.specialize_function_body(then_block, param_to_functor, type_table),
                else_block: else_block
                    .as_ref()
                    .map(|b| self.specialize_function_body(b, param_to_functor, type_table)),
            },
            TirStmtKind::Loop { body } => TirStmtKind::Loop {
                body: self.specialize_function_body(body, param_to_functor, type_table),
            },
            TirStmtKind::LabeledBlock { label, block } => TirStmtKind::LabeledBlock {
                label: label.clone(),
                block: self.specialize_function_body(block, param_to_functor, type_table),
            },
            TirStmtKind::Break { label, value } => TirStmtKind::Break {
                label: label.clone(),
                value: value
                    .as_ref()
                    .map(|v| self.specialize_expr(v, param_to_functor, type_table)),
            },
            TirStmtKind::Continue => TirStmtKind::Continue,
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => TirStmtKind::IfLet {
                scrutinee: self.specialize_expr(scrutinee, param_to_functor, type_table),
                pattern: pattern.clone(),
                then_block: self.specialize_function_body(then_block, param_to_functor, type_table),
                else_block: else_block
                    .as_ref()
                    .map(|b| self.specialize_function_body(b, param_to_functor, type_table)),
            },
            TirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => TirStmtKind::LetDestructure {
                pattern: pattern.clone(),
                is_mut: *is_mut,
                value: self.specialize_expr(value, param_to_functor, type_table),
            },
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        };
        TirStmt::new(kind, stmt.span)
    }

    /// When a fn-param local has been specialized to a functor type and is forwarded
    /// as an argument to another function expecting a function type, wrap it in
    /// `ClosureToCanonical` to convert the functor back to a type-erased canonical closure.
    ///
    /// `original_type_id` is the `type_id` of the argument BEFORE specialization
    /// (from the unmodified body), which retains the original function type.
    fn maybe_wrap_functor_as_canonical(
        &self,
        specialized_arg: TirExpr,
        original_type_id: TypeId,
        param_to_functor: &IndexMap<u32, TypeId>,
        type_table: &TypeTable,
    ) -> TirExpr {
        if let TirExprKind::Local { index, .. } = &specialized_arg.kind
            && let Some(&functor_type) = param_to_functor.get(index)
            && matches!(
                type_table.get(original_type_id),
                ResolvedType::Function { .. }
            )
        {
            // Find the functor_id by matching struct_type_id
            if let Some(functor) = self
                .functor_infos
                .iter()
                .find(|f| f.struct_type_id == functor_type)
            {
                let span = specialized_arg.span;
                return TirExpr::new(
                    TirExprKind::ClosureToCanonical {
                        functor: Box::new(specialized_arg),
                        functor_id: functor.id,
                        target_fn_type: original_type_id,
                        closure_module: self.module_source.clone(),
                    },
                    original_type_id,
                    span,
                );
            }
        }
        specialized_arg
    }

    fn specialize_expr(
        &self,
        expr: &TirExpr,
        param_to_functor: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TirExpr {
        match &expr.kind {
            // Transform IndirectCall on a fn-param local to MethodCall on __call
            TirExprKind::IndirectCall { callee, args } => {
                // Check if callee is a Local that maps to a fn-param with functor type
                if let TirExprKind::Local { index, .. } = &callee.kind
                    && let Some(&functor_type) = param_to_functor.get(index)
                {
                    // Get the functor info to find the __call method name
                    if let Some(functor) = self
                        .functor_infos
                        .iter()
                        .find(|f| f.struct_type_id == functor_type)
                    {
                        let call_method_name =
                            MethodName::format_local(&functor.struct_name, None, "__call");

                        // Transform to MethodCall
                        let new_callee = self.specialize_expr(callee, param_to_functor, type_table);
                        let new_args: Vec<_> = args
                            .iter()
                            .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                            .collect();

                        // Build method_info for the __call method
                        let call_method_info = LocalMethodName::new(
                            functor.struct_name.clone(), // __Closure_N
                            None,                        // no trait
                            "__call".to_string(),        // just the method name
                        );

                        // Use the call_method_name for External ref since codegen expects it
                        let params_is_mut: Vec<bool> = functor
                            .call_method
                            .borrow()
                            .params
                            .iter()
                            .skip(1) // skip self/env param
                            .map(|p| p.is_mut)
                            .collect();
                        let call_args: Vec<CallArg> = new_args
                            .into_iter()
                            .zip(params_is_mut.into_iter().chain(std::iter::repeat(false)))
                            .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                            .collect();
                        return TirExpr::new(
                            TirExprKind::method_call(
                                Box::new(new_callee),
                                FunctionRef {
                                    module_source: self.module_source.clone(),
                                    name: call_method_name,
                                    monomorph_info: None,
                                    method_info: Some(call_method_info),
                                },
                                Vec::new(),
                                call_args,
                            ),
                            expr.type_id,
                            expr.span,
                        );
                    }
                }
                // Not a fn-param local, recurse normally
                TirExpr::new(
                    TirExprKind::IndirectCall {
                        callee: Box::new(self.specialize_expr(
                            callee,
                            param_to_functor,
                            type_table,
                        )),
                        args: args
                            .iter()
                            .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                            .collect(),
                    },
                    expr.type_id,
                    expr.span,
                )
            }
            TirExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
                closure_module,
            } => TirExpr::new(
                TirExprKind::ClosureToCanonical {
                    functor: Box::new(self.specialize_expr(functor, param_to_functor, type_table)),
                    functor_id: *functor_id,
                    target_fn_type: *target_fn_type,
                    closure_module: closure_module.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            // Recurse into sub-expressions
            TirExprKind::Binary { left, op, right } => TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(self.specialize_expr(left, param_to_functor, type_table)),
                    op: *op,
                    right: Box::new(self.specialize_expr(right, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Unary { op, expr: inner } => TirExpr::new(
                TirExprKind::Unary {
                    op: *op,
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Call {
                func,
                args,
                type_args,
            } => TirExpr::new(
                TirExprKind::Call {
                    func: func.clone(),
                    args: args
                        .iter()
                        .map(|a| {
                            let original_type_id = a.expr.type_id;
                            let specialized =
                                self.specialize_expr(&a.expr, param_to_functor, type_table);
                            CallArg::new(
                                self.maybe_wrap_functor_as_canonical(
                                    specialized,
                                    original_type_id,
                                    param_to_functor,
                                    type_table,
                                ),
                                a.is_mut,
                            )
                        })
                        .collect(),
                    type_args: type_args.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                type_args,
                ..
            } => TirExpr::new(
                TirExprKind::method_call(
                    Box::new(self.specialize_expr(receiver, param_to_functor, type_table)),
                    func.clone(),
                    type_args.clone(),
                    args.iter()
                        .map(|a| {
                            let original_type_id = a.expr.type_id;
                            let specialized =
                                self.specialize_expr(&a.expr, param_to_functor, type_table);
                            CallArg::new(
                                self.maybe_wrap_functor_as_canonical(
                                    specialized,
                                    original_type_id,
                                    param_to_functor,
                                    type_table,
                                ),
                                a.is_mut,
                            )
                        })
                        .collect(),
                ),
                expr.type_id,
                expr.span,
            ),
            TirExprKind::CmRawCall { local_name, args } => TirExpr::new(
                TirExprKind::CmRawCall {
                    local_name: local_name.clone(),
                    args: args
                        .iter()
                        .map(|a| self.specialize_expr(a, param_to_functor, type_table))
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Block(block) => TirExpr::new(
                TirExprKind::Block(self.specialize_function_body(
                    block,
                    param_to_functor,
                    type_table,
                )),
                expr.type_id,
                expr.span,
            ),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => TirExpr::new(
                TirExprKind::If {
                    condition: Box::new(self.specialize_expr(
                        condition,
                        param_to_functor,
                        type_table,
                    )),
                    then_branch: self.specialize_function_body(
                        then_branch,
                        param_to_functor,
                        type_table,
                    ),
                    else_branch: else_branch
                        .as_ref()
                        .map(|b| self.specialize_function_body(b, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => TirExpr::new(
                TirExprKind::Cast {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    target_type: *target_type,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                field_name,
            } => TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Index { expr: array, index } => TirExpr::new(
                TirExprKind::Index {
                    expr: Box::new(self.specialize_expr(array, param_to_functor, type_table)),
                    index: Box::new(self.specialize_expr(index, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Assign { target, value } => TirExpr::new(
                TirExprKind::Assign {
                    target: Box::new(self.specialize_expr(target, param_to_functor, type_table)),
                    value: Box::new(self.specialize_expr(value, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => TirExpr::new(
                TirExprKind::Match {
                    expr: Box::new(self.specialize_expr(scrutinee, param_to_functor, type_table)),
                    arms: arms
                        .iter()
                        .map(|arm| TirMatchArm {
                            pattern: arm.pattern.clone(),
                            guard: arm
                                .guard
                                .as_ref()
                                .map(|g| self.specialize_expr(g, param_to_functor, type_table)),
                            body: self.specialize_expr(&arm.body, param_to_functor, type_table),
                            span: arm.span,
                        })
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Closure {
                params,
                body,
                captures,
                functor_id,
                source_text,
                address_taken_locals,
                body_locals,
            } => TirExpr::new(
                TirExprKind::Closure {
                    params: params.clone(),
                    body: Box::new(self.specialize_expr(body, param_to_functor, type_table)),
                    captures: captures.clone(),
                    functor_id: *functor_id,
                    source_text: source_text.clone(),
                    address_taken_locals: address_taken_locals.clone(),
                    body_locals: body_locals.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type: *struct_type,
                    struct_name: struct_name.clone(),
                    fields: fields
                        .iter()
                        .map(|f| TirStructField {
                            name: f.name.clone(),
                            value: self.specialize_expr(&f.value, param_to_functor, type_table),
                            field_index: f.field_index,
                        })
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::TupleLiteral { elements } => TirExpr::new(
                TirExprKind::TupleLiteral {
                    elements: elements
                        .iter()
                        .map(|e| self.specialize_expr(e, param_to_functor, type_table))
                        .collect(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::TupleSpread { expr: inner } => TirExpr::new(
                TirExprKind::TupleSpread {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::TupleZip { expr: inner } => TirExpr::new(
                TirExprKind::TupleZip {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::TypePackExpansion {
                call_expr: inner,
                pack_type_id,
            } => TirExpr::new(
                TirExprKind::TypePackExpansion {
                    call_expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    pack_type_id: *pack_type_id,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type: *variant_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                    payload: payload
                        .as_ref()
                        .map(|p| Box::new(self.specialize_expr(p, param_to_functor, type_table))),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => TirExpr::new(
                TirExprKind::LabeledBlock {
                    label: label.clone(),
                    block: self.specialize_function_body(block, param_to_functor, type_table),
                    result_type: *result_type,
                },
                expr.type_id,
                expr.span,
            ),
            // Terminals - clone as-is
            TirExprKind::Local { index, name } => {
                // Update type if this local is a fn-param with functor type
                let type_id = if let Some(&functor_type) = param_to_functor.get(index) {
                    type_table.make_ref(functor_type)
                } else {
                    expr.type_id
                };
                TirExpr::new(
                    TirExprKind::Local {
                        index: *index,
                        name: name.clone(),
                    },
                    type_id,
                    expr.span,
                )
            }
            TirExprKind::IntLiteral { value, repr } => TirExpr::new(
                TirExprKind::IntLiteral {
                    value: *value,
                    repr: repr.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::FloatLiteral { value, repr } => TirExpr::new(
                TirExprKind::FloatLiteral {
                    value: *value,
                    repr: repr.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::BoolLiteral(v) => {
                TirExpr::new(TirExprKind::BoolLiteral(*v), expr.type_id, expr.span)
            }
            TirExprKind::CharLiteral(c) => {
                TirExpr::new(TirExprKind::CharLiteral(*c), expr.type_id, expr.span)
            }
            TirExprKind::StringLiteral(s) => TirExpr::new(
                TirExprKind::StringLiteral(s.clone()),
                expr.type_id,
                expr.span,
            ),
            TirExprKind::BytesLiteral(b) => TirExpr::new(
                TirExprKind::BytesLiteral(b.clone()),
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Null => TirExpr::new(TirExprKind::Null, expr.type_id, expr.span),
            TirExprKind::Unit => TirExpr::new(TirExprKind::Unit, expr.type_id, expr.span),
            TirExprKind::FuncRef {
                name,
                module_source,
            } => TirExpr::new(
                TirExprKind::FuncRef {
                    name: name.clone(),
                    module_source: module_source.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::GlobalVarGet {
                name,
                module_source,
            } => TirExpr::new(
                TirExprKind::GlobalVarGet {
                    name: name.clone(),
                    module_source: module_source.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::GlobalVarSet {
                name,
                module_source,
                value,
            } => TirExpr::new(
                TirExprKind::GlobalVarSet {
                    name: name.clone(),
                    module_source: module_source.clone(),
                    value: Box::new(self.specialize_expr(value, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Capture { index, name } => TirExpr::new(
                TirExprKind::Capture {
                    index: *index,
                    name: name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::EnumConstruct {
                enum_type,
                case_index,
                case_name,
            } => TirExpr::new(
                TirExprKind::EnumConstruct {
                    enum_type: *enum_type,
                    case_index: *case_index,
                    case_name: case_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantTag { expr: inner } => TirExpr::new(
                TirExprKind::VariantTag {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantTest {
                expr: inner,
                case_index,
                case_name,
            } => TirExpr::new(
                TirExprKind::VariantTest {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    case_index: *case_index,
                    case_name: case_name.clone(),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::VariantPayload {
                expr: inner,
                case_index,
                payload_type,
            } => TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(self.specialize_expr(inner, param_to_functor, type_table)),
                    case_index: *case_index,
                    payload_type: *payload_type,
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => TirExpr::new(
                TirExprKind::Switch {
                    scrutinee: Box::new(self.specialize_expr(
                        scrutinee,
                        param_to_functor,
                        type_table,
                    )),
                    min_value: *min_value,
                    arms: arms
                        .iter()
                        .map(|arm| self.specialize_function_body(arm, param_to_functor, type_table))
                        .collect(),
                    default: self.specialize_function_body(default, param_to_functor, type_table),
                },
                expr.type_id,
                expr.span,
            ),
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }

    fn transform_block(&mut self, block: &mut TirBlock, type_table: &mut TypeTable) {
        for stmt in &mut block.stmts {
            self.transform_stmt(stmt, type_table);
        }
    }

    fn transform_stmt(&mut self, stmt: &mut TirStmt, type_table: &mut TypeTable) {
        match &mut stmt.kind {
            TirStmtKind::Let {
                local_index,
                value,
                type_id,
                ..
            } => {
                // Track if this local stores a closure (read the stable ID
                // off the Closure node).
                let closure_id = if let TirExprKind::Closure {
                    functor_id: Some(id),
                    ..
                } = &value.kind
                {
                    self.local_to_closure.insert(*local_index, *id);
                    Some(*id)
                } else {
                    None
                };

                self.transform_expr(value, type_table);

                // Update the Let statement's type_id for specializable closures
                // (non-specializable closures keep their fn(...) type for ClosureToCanonical)
                if let Some(id) = closure_id
                    && self.specializable.contains(&id)
                    && let Some(functor) = self.functor_infos.get(id as usize)
                {
                    *type_id = functor.ref_type_id;
                }
            }
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                self.transform_expr(expr, type_table);
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
            TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.transform_expr(value, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.transform_expr(condition, type_table);
                self.transform_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.transform_block(else_blk, type_table);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.transform_block(body, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.transform_expr(scrutinee, type_table);
                self.transform_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.transform_block(else_blk, type_table);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.transform_expr(value, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    fn transform_expr(&mut self, expr: &mut TirExpr, type_table: &mut TypeTable) {
        match &mut expr.kind {
            TirExprKind::Closure {
                captures,
                functor_id,
                ..
            } => {
                // The closure's ID was assigned in the collect pass and
                // lives on the node. Reading it here means we don't have
                // to keep a counter in sync with traversal order — which
                // would break the moment we walk a different set of
                // functions (e.g. the generated `__call` methods).
                let closure_id = functor_id.unwrap_or_else(|| {
                    panic!(
                        "Closure node missing functor_id; the collect pass should assign it (span: {:?})",
                        expr.span,
                    )
                });

                // Don't recurse into the body. The body lives in *this*
                // closure's local-index namespace, not the function we
                // are walking, so descending here would corrupt
                // `local_to_closure` (used by `update_local_types` and
                // by IndirectCall→MethodCall rewriting). The closure's
                // body lives independently in the generated `__call`
                // method, which `lower_module` walks alongside the
                // original module functions in this same pass.

                // Specializable closures: transform to StructLiteral (stored in local, called directly)
                // Non-specializable closures: set functor_id and leave as Closure for fn-param specialization
                // The try_transform_fn_param_call will handle specialization. Any remaining
                // Closure nodes after that are transformed to ClosureToCanonical in a final pass.
                if self.specializable.contains(&closure_id)
                    && let Some(functor) = self.functor_infos.get(closure_id as usize)
                {
                    // Transform Closure to StructLiteral
                    let struct_name = functor.struct_name.clone();

                    // Build field expressions from captures
                    let fields: Vec<TirStructField> = captures
                        .iter()
                        .enumerate()
                        .map(|(i, cap)| TirStructField {
                            name: format!("__capture_{i}"),
                            value: TirExpr::new(
                                TirExprKind::Local {
                                    index: cap.outer_index,
                                    name: cap.name.clone(),
                                },
                                cap.type_id,
                                expr.span,
                            ),
                            field_index: i as u32,
                        })
                        .collect();

                    // Replace with StructLiteral
                    // struct_type uses bare struct type for codegen
                    // type_id uses ref type because functors are reference types
                    expr.kind = TirExprKind::StructLiteral {
                        struct_type: functor.struct_type_id,
                        struct_name,
                        fields,
                    };
                    expr.type_id = functor.ref_type_id;
                }
                // For non-specializable closures (passed as arguments), the
                // node's pre-assigned `functor_id` already points at the
                // corresponding ClosureFunctor — `try_transform_fn_param_call`
                // and the phase-4 `ClosureToCanonical` rewrite both read it
                // off the node directly.
            }
            TirExprKind::IndirectCall { callee, args } => {
                // Transform callee and args first
                self.transform_expr(callee, type_table);
                for arg in &mut *args {
                    self.transform_expr(arg, type_table);
                }

                // Check if callee is a local that stores a safe-to-transform closure
                if let TirExprKind::Local { index, .. } = &callee.kind
                    && let Some(closure_id) = self.local_to_closure.get(index)
                    && self.specializable.contains(closure_id)
                    && let Some(functor) = self.functor_infos.get(*closure_id as usize)
                {
                    // Transform IndirectCall to MethodCall
                    // Update callee's type to the functor ref type (functors are reference types)
                    let mut callee_owned = std::mem::replace(
                        callee.as_mut(),
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                    );
                    callee_owned.type_id = functor.ref_type_id;

                    // Get return type from the __call method
                    let return_type = functor.call_method.borrow().return_type;

                    // Replace with MethodCall using FunctionRef::from_resolved
                    let args_owned: Vec<TirExpr> = std::mem::take(args);
                    let params_is_mut: Vec<bool> = functor
                        .call_method
                        .borrow()
                        .params
                        .iter()
                        .skip(1) // skip self/env param
                        .map(|p| p.is_mut)
                        .collect();
                    let call_args: Vec<CallArg> = args_owned
                        .into_iter()
                        .zip(params_is_mut.into_iter().chain(std::iter::repeat(false)))
                        .map(|(expr, is_mut)| CallArg::new(expr, is_mut))
                        .collect();
                    expr.kind = TirExprKind::method_call(
                        Box::new(callee_owned),
                        FunctionRef::from_resolved(
                            &functor.call_method.borrow(),
                            self.module_source.clone(),
                        ),
                        Vec::new(),
                        call_args,
                    );
                    expr.type_id = return_type;
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.transform_expr(functor, type_table);
            }
            // Recursive cases - transform all sub-expressions
            TirExprKind::Binary { left, right, .. } => {
                self.transform_expr(left, type_table);
                self.transform_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                self.transform_expr(inner, type_table);
            }
            TirExprKind::Call { func, args, .. } => {
                for arg in &mut *args {
                    self.transform_expr(&mut arg.expr, type_table);
                }
                // Check if this call has closure arguments that need fn-param specialization
                self.try_transform_fn_param_call(func, args, type_table);
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.transform_expr(arg, type_table);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                type_args: _,
                ..
            } => {
                self.transform_expr(receiver, type_table);
                for arg in &mut *args {
                    self.transform_expr(&mut arg.expr, type_table);
                }

                // Check if this call has closure arguments that need fn-param specialization
                self.try_transform_fn_param_call(func, args, type_table);
            }
            TirExprKind::Block(block) => {
                self.transform_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.transform_expr(condition, type_table);
                self.transform_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.transform_block(else_blk, type_table);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.transform_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.transform_expr(elem, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.transform_expr(target, type_table);
                self.transform_expr(value, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.transform_expr(array, type_table);
                self.transform_expr(index, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.transform_expr(scrutinee, type_table);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.transform_expr(guard, type_table);
                    }
                    self.transform_expr(&mut arm.body, type_table);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.transform_expr(payload_expr, type_table);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.transform_block(block, type_table);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.transform_expr(value, type_table);
            }
            // Lowered pattern matching nodes
            TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. }
            | TirExprKind::VariantPayload { expr, .. } => {
                self.transform_expr(expr, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.transform_expr(scrutinee, type_table);
                for arm in arms {
                    self.transform_block(arm, type_table);
                }
                self.transform_block(default, type_table);
            }
            // Terminals - nothing to transform
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }

    /// Try to transform a call with closure arguments to use a specialized function.
    /// This handles fn-param monomorphization: when a closure is passed to a fn-type parameter,
    /// we transform the closure to a struct literal and change the call to use the specialized function.
    fn try_transform_fn_param_call(
        &self,
        func: &mut FunctionRef,
        args: &mut [CallArg],
        type_table: &mut TypeTable,
    ) {
        // Collect closure args that have functor_id (meaning they were passed as fn-type params)
        let mut functor_types = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if let TirExprKind::Closure {
                functor_id: Some(id),
                ..
            } = &arg.expr.kind
                && let Some(functor) = self.functor_infos.get(*id as usize)
            {
                functor_types.push((i as u32, functor.struct_type_id));
            }
        }

        if functor_types.is_empty() {
            return;
        }

        // Build spec key and look up specialized function
        let key = FnParamSpecKey {
            callee_name: func.name.clone(),
            functor_types: functor_types.clone(),
        };

        let Some(specialized_name) = self.fn_param_specializations.get(&key) else {
            return;
        };

        // Transform closure args to struct literals
        for (arg_idx, _functor_type_id) in &functor_types {
            let arg = &mut args[*arg_idx as usize];
            if let TirExprKind::Closure {
                captures,
                functor_id: Some(closure_id),
                ..
            } = &arg.expr.kind
                && let Some(functor) = self.functor_infos.get(*closure_id as usize)
            {
                // Build struct fields from captures
                let fields: Vec<TirStructField> = captures
                    .iter()
                    .enumerate()
                    .map(|(i, cap)| TirStructField {
                        name: format!("__capture_{i}"),
                        value: TirExpr::new(
                            TirExprKind::Local {
                                index: cap.outer_index,
                                name: cap.name.clone(),
                            },
                            cap.type_id,
                            arg.expr.span,
                        ),
                        field_index: i as u32,
                    })
                    .collect();

                // Transform to struct literal (functors are reference types, no Move needed)
                arg.expr.kind = TirExprKind::StructLiteral {
                    struct_type: functor.struct_type_id,
                    struct_name: functor.struct_name.clone(),
                    fields,
                };
                arg.expr.type_id = functor.ref_type_id;
            }
        }

        // Build the functor suffix for the specialized method name
        let functor_suffix: String =
            functor_types
                .iter()
                .fold(String::new(), |mut acc, (_, tid)| {
                    let name = type_table.type_name(*tid);
                    let _ = write!(acc, "${name}");
                    acc
                });

        // Build specialized method_info with the specialized method name
        // The functor suffix goes AFTER type args, so we use full_method_name()
        // and then clear method_type_args to avoid duplication
        let specialized_method_info = func.method_info.clone().map(|info| {
            LocalMethodName {
                struct_name: info.struct_name.clone(),
                base_struct_name: info.base_struct_name.clone(),
                trait_name: info.trait_name.clone(),
                base_trait_name: info.base_trait_name.clone(),
                trait_type_args: info.trait_type_args.clone(),
                method_name: format!("{}{}", info.full_method_name(), functor_suffix),
                method_type_args: Vec::new(), // Type args are now in method_name
                is_type_param_receiver: info.is_type_param_receiver,
                is_ref_impl: false,
                cm_name: info.cm_name,
            }
        });

        // Update the function reference to use the specialized name
        // Use self.module_source so it matches the specialized function definition
        // Preserve monomorph_info so DCE can trace the call graph correctly
        let orig_monomorph_info = func.monomorph_info.clone();
        *func = FunctionRef {
            module_source: self.module_source.clone(),
            name: specialized_name.clone(),
            monomorph_info: orig_monomorph_info,
            method_info: specialized_method_info,
        };
    }

    /// Transform remaining Closure nodes to `ClosureToCanonical`.
    /// This is called after fn-param specialization has had a chance to transform closures.
    /// Any Closure nodes still remaining are those where specialization was skipped
    /// (e.g., fn-param stored in struct field).
    fn transform_remaining_closures_block(&self, block: &mut TirBlock) {
        for stmt in &mut block.stmts {
            self.transform_remaining_closures_stmt(stmt);
        }
    }

    fn transform_remaining_closures_stmt(&self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.transform_remaining_closures_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.transform_remaining_closures_expr(expr);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                self.transform_remaining_closures_expr(expr);
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
            TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    self.transform_remaining_closures_expr(value);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.transform_remaining_closures_expr(condition);
                self.transform_remaining_closures_block(then_block);
                if let Some(else_blk) = else_block {
                    self.transform_remaining_closures_block(else_blk);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.transform_remaining_closures_block(body);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.transform_remaining_closures_expr(scrutinee);
                self.transform_remaining_closures_block(then_block);
                if let Some(else_blk) = else_block {
                    self.transform_remaining_closures_block(else_blk);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.transform_remaining_closures_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    fn transform_remaining_closures_expr(&self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Closure {
                captures,
                functor_id: Some(closure_id),
                ..
            } => {
                // Don't recurse into the body — it lives in this closure's
                // local-index namespace. Its own clone in the generated
                // `__call` method is walked by `lower_module` in this same
                // pass (see the `lowered_funcs` set).

                // This closure wasn't specialized, transform to ClosureToCanonical
                if let Some(functor) = self.functor_infos.get(*closure_id as usize) {
                    let struct_name = functor.struct_name.clone();

                    // Build field expressions from captures
                    let fields: Vec<TirStructField> = captures
                        .iter()
                        .enumerate()
                        .map(|(i, cap)| TirStructField {
                            name: format!("__capture_{i}"),
                            value: TirExpr::new(
                                TirExprKind::Local {
                                    index: cap.outer_index,
                                    name: cap.name.clone(),
                                },
                                cap.type_id,
                                expr.span,
                            ),
                            field_index: i as u32,
                        })
                        .collect();

                    // Build the StructLiteral (functors are reference types)
                    let struct_literal = TirExpr::new(
                        TirExprKind::StructLiteral {
                            struct_type: functor.struct_type_id,
                            struct_name,
                            fields,
                        },
                        functor.ref_type_id,
                        expr.span,
                    );

                    // Wrap in ClosureToCanonical
                    let target_fn_type = expr.type_id; // Original function type
                    expr.kind = TirExprKind::ClosureToCanonical {
                        functor: Box::new(struct_literal),
                        functor_id: *closure_id,
                        target_fn_type,
                        closure_module: self.module_source.clone(),
                    };
                    // Keep original function type for type compatibility
                }
            }
            // Recurse into all expression kinds
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.transform_remaining_closures_expr(&mut arg.expr);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.transform_remaining_closures_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.transform_remaining_closures_expr(receiver);
                for arg in args {
                    self.transform_remaining_closures_expr(&mut arg.expr);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.transform_remaining_closures_expr(left);
                self.transform_remaining_closures_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
                self.transform_remaining_closures_expr(inner);
            }
            TirExprKind::Assign { target, value } => {
                self.transform_remaining_closures_expr(target);
                self.transform_remaining_closures_expr(value);
            }
            TirExprKind::Index { expr: arr, index } => {
                self.transform_remaining_closures_expr(arr);
                self.transform_remaining_closures_expr(index);
            }
            TirExprKind::Block(block) => {
                self.transform_remaining_closures_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.transform_remaining_closures_expr(condition);
                self.transform_remaining_closures_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.transform_remaining_closures_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.transform_remaining_closures_expr(&mut field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.transform_remaining_closures_expr(elem);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.transform_remaining_closures_expr(callee);
                for arg in args {
                    self.transform_remaining_closures_expr(arg);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.transform_remaining_closures_expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.transform_remaining_closures_expr(guard);
                    }
                    self.transform_remaining_closures_expr(&mut arm.body);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                self.transform_remaining_closures_expr(p);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.transform_remaining_closures_block(block);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.transform_remaining_closures_expr(value);
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.transform_remaining_closures_expr(functor);
            }
            TirExprKind::Closure {
                body,
                functor_id: None,
                ..
            } => {
                // Closure without functor_id - just recurse into body
                self.transform_remaining_closures_expr(body);
            }
            TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                self.transform_remaining_closures_expr(inner);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.transform_remaining_closures_expr(scrutinee);
                for arm in arms {
                    self.transform_remaining_closures_block(arm);
                }
                self.transform_remaining_closures_block(default);
            }
            // Leaf nodes - nothing to recurse into
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::VariantConstruct { payload: None, .. } => {}
            TirExprKind::TemplateString { .. } => {
                unreachable!("TemplateString should be expanded before this phase")
            }
            TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
                unreachable!(
                    "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
                )
            }
        }
    }
}
