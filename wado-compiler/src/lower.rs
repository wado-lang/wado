//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - String literal collection (for data section)
//! - Reactive signal dependency graph construction
//! - Method call resolution (direct vs. effect operation)
//! - Generic instantiation / monomorphization
//! - Closure capture analysis
//! - JSX element type binding (future)

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::tir::{
    InstantiationKey, MonomorphInfo, ResolvedType, TirBlock, TirExpr, TirExprKind, TirField,
    TirFunction, TirModule, TirParam, TirStmt, TirStmtKind, TirStruct, TypeId, TypeTable,
};

/// Lower a TIR module
///
/// Currently performs:
/// - Monomorphization of generic types
/// - String literal collection
pub fn lower(mut module: TirModule) -> TirModule {
    // Perform monomorphization
    let mut monomorph = Monomorphizer::new();
    module = monomorph.monomorphize(module);

    // Collect string literals
    let mut collector = StringCollector::new();
    collector.collect_module(&module);
    module.string_literals = collector.into_strings();

    module
}

/// Lower multiple modules
pub fn lower_modules(modules: Vec<TirModule>) -> Vec<TirModule> {
    modules.into_iter().map(lower).collect()
}

/// Lower multiple modules with cross-module generic function support
///
/// This function enables monomorphization of generic functions defined in one module
/// but used in another (e.g., Array methods from prelude used in user code).
///
/// IMPORTANT: Requires unified type tables - all modules must share the same TypeTable
/// so that TypeIds are valid across modules.
pub fn lower_modules_indexed(
    modules: IndexMap<Vec<String>, TirModule>,
) -> IndexMap<Vec<String>, TirModule> {
    // First pass: collect all generic functions from all modules
    let mut all_generic_functions: HashMap<String, TirFunction> = HashMap::new();
    for module in modules.values() {
        for func in &module.functions {
            if !func.type_params.is_empty() || !func.impl_type_params.is_empty() {
                all_generic_functions.insert(func.name.clone(), func.clone());
            }
        }
    }

    // Second pass: lower each module using the combined generic functions
    modules
        .into_iter()
        .map(|(path, module)| {
            (
                path,
                lower_with_cross_module_generics(module, &all_generic_functions),
            )
        })
        .collect()
}

/// Lower a single module with access to cross-module generic functions
fn lower_with_cross_module_generics(
    mut module: TirModule,
    all_generic_functions: &HashMap<String, TirFunction>,
) -> TirModule {
    // Perform monomorphization with cross-module generic function support
    let mut monomorph = Monomorphizer::new();
    module = monomorph.monomorphize_with_externals(module, all_generic_functions);

    // Collect string literals
    let mut collector = StringCollector::new();
    collector.collect_module(&module);
    module.string_literals = collector.into_strings();

    module
}

// ============================================================================
// Monomorphization
// ============================================================================

/// Monomorphizer collects generic instantiations and generates concrete types
struct Monomorphizer {
    /// Map from (generic_name, type_args) to mangled name for structs
    instantiated: HashMap<InstantiationKey, String>,
    /// Work queue of pending struct instantiations
    pending: Vec<InstantiationKey>,
    /// Map from GenericInstance TypeId to monomorphized Struct TypeId
    type_substitutions: HashMap<TypeId, TypeId>,
    /// Map from GenericInstance TypeId to mangled struct name
    type_to_mangled_name: HashMap<TypeId, String>,
    /// Map from (generic_func_name, type_args) to mangled function name
    function_instantiated: HashMap<InstantiationKey, String>,
    /// Work queue of pending function instantiations
    function_pending: Vec<InstantiationKey>,
}

impl Monomorphizer {
    fn new() -> Self {
        Self {
            instantiated: HashMap::new(),
            pending: Vec::new(),
            type_substitutions: HashMap::new(),
            type_to_mangled_name: HashMap::new(),
            function_instantiated: HashMap::new(),
            function_pending: Vec::new(),
        }
    }

    /// Perform monomorphization on a module
    fn monomorphize(&mut self, mut module: TirModule) -> TirModule {
        // ========================
        // Struct Monomorphization
        // ========================

        // Phase 1: Collect all generic struct definitions
        let generic_structs: HashMap<String, TirStruct> = module
            .structs
            .iter()
            .filter(|s| !s.type_params.is_empty())
            .map(|s| (s.name.clone(), s.clone()))
            .collect();

        // Store in module for later phases
        module.generic_structs = generic_structs.clone();

        // Phase 2: Collect all struct instantiation sites from the type table
        self.collect_instantiation_sites(&module.type_table);

        // Phase 3: Process struct instantiations and generate concrete structs
        let mut new_structs = Vec::new();
        while let Some(key) = self.pending.pop() {
            if let Some(generic_struct) = generic_structs.get(&key.name)
                && let Some(concrete) =
                    self.instantiate_struct(generic_struct, &key, &mut module.type_table)
            {
                new_structs.push(concrete);
            }
        }

        // Phase 4: Add monomorphized structs to module
        module.structs.extend(new_structs);

        // Phase 5: Remove generic structs from the concrete struct list
        // (they stay in generic_structs for reference)
        module
            .structs
            .retain(|s| s.type_params.is_empty() || s.monomorph_info.is_some());

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module);

        // ============================
        // Function Monomorphization
        // ============================

        // Phase 7: Collect all generic function definitions
        // Include both functions with method-level type params AND methods from generic impl blocks
        let generic_functions: HashMap<String, TirFunction> = module
            .functions
            .iter()
            .filter(|f| !f.type_params.is_empty() || !f.impl_type_params.is_empty())
            .map(|f| (f.name.clone(), f.clone()))
            .collect();

        // Store in module for later phases
        module.generic_functions = generic_functions.clone();

        // Phase 8: Collect function instantiation sites from Call expressions
        self.collect_function_instantiation_sites(&module, &generic_functions);

        // Phase 9: Process function instantiations and generate concrete functions
        let mut new_functions = Vec::new();
        while let Some(key) = self.function_pending.pop() {
            if let Some(generic_func) = generic_functions.get(&key.name)
                && let Some(concrete) =
                    self.instantiate_function(generic_func, &key, &mut module.type_table)
            {
                new_functions.push(concrete);
            }
        }

        // Phase 10: Add monomorphized functions to module
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        // (they stay in generic_functions for reference)
        // Remove functions with type_params OR impl_type_params (unless monomorphized)
        module.functions.retain(|f| {
            (f.type_params.is_empty() && f.impl_type_params.is_empty())
                || f.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        module
    }

    /// Perform monomorphization with access to external generic functions
    ///
    /// This enables monomorphization of generic functions defined in other modules
    /// (e.g., Array methods from prelude used in user code).
    ///
    /// IMPORTANT: Requires unified type tables - TypeIds in external_generic_functions
    /// must be valid in the module's type_table.
    fn monomorphize_with_externals(
        &mut self,
        mut module: TirModule,
        external_generic_functions: &HashMap<String, TirFunction>,
    ) -> TirModule {
        // ========================
        // Struct Monomorphization
        // ========================

        // Phase 1: Collect all generic struct definitions
        let generic_structs: HashMap<String, TirStruct> = module
            .structs
            .iter()
            .filter(|s| !s.type_params.is_empty())
            .map(|s| (s.name.clone(), s.clone()))
            .collect();

        // Store in module for later phases
        module.generic_structs = generic_structs.clone();

        // Phase 2: Collect all struct instantiation sites from the type table
        self.collect_instantiation_sites(&module.type_table);

        // Phase 3: Process struct instantiations and generate concrete structs
        let mut new_structs = Vec::new();
        while let Some(key) = self.pending.pop() {
            if let Some(generic_struct) = generic_structs.get(&key.name)
                && let Some(concrete) =
                    self.instantiate_struct(generic_struct, &key, &mut module.type_table)
            {
                new_structs.push(concrete);
            }
        }

        // Phase 4: Add monomorphized structs to module
        module.structs.extend(new_structs);

        // Phase 5: Remove generic structs from the concrete struct list
        module
            .structs
            .retain(|s| s.type_params.is_empty() || s.monomorph_info.is_some());

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module);

        // ============================
        // Function Monomorphization
        // ============================

        // Phase 7: Collect all generic function definitions
        // Include both local functions AND external generic functions from other modules
        let mut generic_functions: HashMap<String, TirFunction> =
            external_generic_functions.clone();

        // Local generic functions override external ones (allows module-local specialization)
        for func in &module.functions {
            if !func.type_params.is_empty() || !func.impl_type_params.is_empty() {
                generic_functions.insert(func.name.clone(), func.clone());
            }
        }

        // Store in module for later phases
        module.generic_functions = generic_functions.clone();

        // Phase 8: Collect function instantiation sites from Call expressions
        self.collect_function_instantiation_sites(&module, &generic_functions);

        // Phase 9: Process function instantiations and generate concrete functions
        let mut new_functions = Vec::new();
        while let Some(key) = self.function_pending.pop() {
            if let Some(generic_func) = generic_functions.get(&key.name)
                && let Some(concrete) =
                    self.instantiate_function(generic_func, &key, &mut module.type_table)
            {
                new_functions.push(concrete);
            }
        }

        // Phase 10: Add monomorphized functions to module
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        // Remove functions with type_params OR impl_type_params (unless monomorphized)
        module.functions.retain(|f| {
            (f.type_params.is_empty() && f.impl_type_params.is_empty())
                || f.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        module
    }

    /// Rewrite all GenericInstance type_ids in expressions to use monomorphized struct types
    fn rewrite_types_in_module(&self, module: &mut TirModule) {
        // Rewrite struct field types
        for strct in &mut module.structs {
            for field in &mut strct.fields {
                field.type_id = self.rewrite_type_id(field.type_id, &mut module.type_table);
            }
        }

        // Rewrite function signatures and bodies
        for func in &mut module.functions {
            // Rewrite function parameters
            for param in &mut func.params {
                param.type_id = self.rewrite_type_id(param.type_id, &mut module.type_table);
            }
            // Rewrite return type
            func.return_type = self.rewrite_type_id(func.return_type, &mut module.type_table);
            // Rewrite local_types
            for local_type in &mut func.local_types {
                *local_type = self.rewrite_type_id(*local_type, &mut module.type_table);
            }
            // Rewrite function body
            if let Some(body) = &mut func.body {
                self.rewrite_types_in_block(body, &mut module.type_table);
            }
        }
    }

    fn rewrite_types_in_block(&self, block: &mut TirBlock, type_table: &mut TypeTable) {
        for stmt in &mut block.stmts {
            self.rewrite_types_in_stmt(stmt, type_table);
        }
    }

    fn rewrite_types_in_stmt(&self, stmt: &mut TirStmt, type_table: &mut TypeTable) {
        match &mut stmt.kind {
            TirStmtKind::Let { type_id, value, .. } => {
                *type_id = self.rewrite_type_id(*type_id, type_table);
                self.rewrite_types_in_expr(value, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.rewrite_types_in_expr(expr, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.rewrite_types_in_expr(expr, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.rewrite_types_in_expr(condition, type_table);
                self.rewrite_types_in_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.rewrite_types_in_block(else_blk, type_table);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.rewrite_types_in_expr(condition, type_table);
                self.rewrite_types_in_block(body, type_table);
            }
            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                if let Some(cond) = condition {
                    self.rewrite_types_in_expr(cond, type_table);
                }
                self.rewrite_types_in_block(body, type_table);
                if let Some(upd) = update {
                    self.rewrite_types_in_expr(upd, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.rewrite_types_in_block(body, type_table);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.rewrite_types_in_expr(iterable, type_table);
                self.rewrite_types_in_block(body, type_table);
            }
            TirStmtKind::Break | TirStmtKind::Continue => {}
            TirStmtKind::Assert {
                condition,
                message,
                intermediates,
                ..
            } => {
                self.rewrite_types_in_expr(condition, type_table);
                if let Some(msg) = message {
                    self.rewrite_types_in_expr(msg, type_table);
                }
                for (_, expr, _) in intermediates {
                    self.rewrite_types_in_expr(expr, type_table);
                }
            }
        }
    }

    fn rewrite_types_in_expr(&self, expr: &mut TirExpr, type_table: &mut TypeTable) {
        // Rewrite the expression's own type_id
        expr.type_id = self.rewrite_type_id(expr.type_id, type_table);

        // Recursively rewrite types in sub-expressions
        match &mut expr.kind {
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                let original_type_id = *struct_type;
                let new_type_id = self.rewrite_type_id(original_type_id, type_table);
                *struct_type = new_type_id;
                // Update struct_name if it was monomorphized (use original type_id to look up)
                if let Some(mangled_name) = self.type_to_mangled_name.get(&original_type_id) {
                    *struct_name = mangled_name.clone();
                }
                for field in fields {
                    self.rewrite_types_in_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.rewrite_types_in_expr(left, type_table);
                self.rewrite_types_in_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.rewrite_types_in_expr(inner, type_table);
            }
            TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.rewrite_types_in_expr(arg, type_table);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.rewrite_types_in_expr(receiver, type_table);
                for arg in args {
                    self.rewrite_types_in_expr(arg, type_table);
                }
            }
            TirExprKind::EffectCall { args, .. } => {
                for arg in args {
                    self.rewrite_types_in_expr(arg, type_table);
                }
            }
            TirExprKind::Block(block) => {
                self.rewrite_types_in_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.rewrite_types_in_expr(condition, type_table);
                self.rewrite_types_in_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.rewrite_types_in_block(else_blk, type_table);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.rewrite_types_in_expr(elem, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.rewrite_types_in_expr(target, type_table);
                self.rewrite_types_in_expr(value, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.rewrite_types_in_expr(inner, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.rewrite_types_in_expr(inner, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.rewrite_types_in_expr(array, type_table);
                self.rewrite_types_in_expr(index, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.rewrite_types_in_expr(scrutinee, type_table);
                for arm in arms {
                    self.rewrite_types_in_expr(&mut arm.body, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.rewrite_types_in_expr(body, type_table);
            }
            // Literals and simple expressions don't need rewriting
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::Capture { .. } => {}
            TirExprKind::IndirectCall { callee, args } => {
                self.rewrite_types_in_expr(callee, type_table);
                for arg in args {
                    self.rewrite_types_in_expr(arg, type_table);
                }
            }
        }
    }

    /// Rewrite a single type_id: if it's a GenericInstance, return the concrete struct type_id.
    /// Also handles container types (Array, Option, Tuple) that may contain GenericInstance.
    fn rewrite_type_id(&self, type_id: TypeId, type_table: &mut TypeTable) -> TypeId {
        // First check direct substitution
        if let Some(&new_id) = self.type_substitutions.get(&type_id) {
            return new_id;
        }

        // Handle container types that may contain GenericInstance
        match type_table.get(type_id).clone() {
            ResolvedType::Option(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id != inner_id {
                    type_table.make_option(new_inner_id)
                } else {
                    type_id
                }
            }
            ResolvedType::Tuple(elem_ids) => {
                let new_elem_ids: Vec<TypeId> = elem_ids
                    .iter()
                    .map(|&id| self.rewrite_type_id(id, type_table))
                    .collect();
                if new_elem_ids != elem_ids {
                    type_table.make_tuple(new_elem_ids)
                } else {
                    type_id
                }
            }
            ResolvedType::Result { ok, err } => {
                let new_ok = self.rewrite_type_id(ok, type_table);
                let new_err = self.rewrite_type_id(err, type_table);
                if new_ok != ok || new_err != err {
                    type_table.make_result(new_ok, new_err)
                } else {
                    type_id
                }
            }
            _ => type_id,
        }
    }

    /// Collect all GenericInstance types from the type table
    fn collect_instantiation_sites(&mut self, type_table: &TypeTable) {
        for id in 0..type_table.len() as TypeId {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(id)
            {
                // Only process if all type args are concrete (no TypeParams)
                let all_concrete = type_args
                    .iter()
                    .all(|&arg| !type_table.contains_type_param(arg));

                if all_concrete {
                    let key = InstantiationKey {
                        name: name.clone(),
                        type_args: type_args.clone(),
                    };

                    if !self.instantiated.contains_key(&key) {
                        let mangled = self.mangle_name(&key, type_table);
                        self.instantiated.insert(key.clone(), mangled);
                        self.pending.push(key);
                    }
                }
            }
        }
    }

    /// Generate mangled name for instantiation: Box<i32> -> Box$i32
    fn mangle_name(&self, key: &InstantiationKey, type_table: &TypeTable) -> String {
        let mut name = key.name.clone();
        for type_arg in &key.type_args {
            name.push('$');
            name.push_str(&self.type_id_to_name_component(*type_arg, type_table));
        }
        name
    }

    /// Convert a TypeId to a mangled name component
    fn type_id_to_name_component(&self, type_id: TypeId, type_table: &TypeTable) -> String {
        match type_table.get(type_id) {
            ResolvedType::Primitive(p) => format!("{:?}", p).to_lowercase(),
            ResolvedType::String => "String".to_string(),
            ResolvedType::Unit => "unit".to_string(),
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::Option(inner) => {
                format!(
                    "Option${}",
                    self.type_id_to_name_component(*inner, type_table)
                )
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let mut result = name.clone();
                for arg in type_args {
                    result.push('$');
                    result.push_str(&self.type_id_to_name_component(*arg, type_table));
                }
                result
            }
            _ => format!("T{}", type_id),
        }
    }

    /// Instantiate a generic struct with concrete type arguments
    fn instantiate_struct(
        &mut self,
        generic: &TirStruct,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirStruct> {
        let mangled_name = self.instantiated.get(key)?.clone();

        // Build substitution map: type param index -> concrete type
        let substitution: HashMap<u32, TypeId> = generic
            .type_params
            .iter()
            .zip(key.type_args.iter())
            .map(|(param, &arg)| (param.index, arg))
            .collect();

        // Substitute types in fields
        let fields: Vec<TirField> = generic
            .fields
            .iter()
            .map(|field| {
                let new_type_id = self.substitute_type(field.type_id, &substitution, type_table);
                TirField {
                    name: field.name.clone(),
                    type_id: new_type_id,
                    index: field.index,
                    span: field.span,
                }
            })
            .collect();

        // Create the monomorphized struct
        let concrete = TirStruct {
            name: mangled_name.clone(),
            is_pub: generic.is_pub,
            type_params: vec![], // Concrete struct has no type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                type_args: key.type_args.clone(),
            }),
            fields,
            span: generic.span,
        };

        // Register the concrete struct type in the type table
        let concrete_type_id = type_table.make_struct(mangled_name, vec![]);

        // Find the GenericInstance TypeId and record the substitution
        for id in 0..type_table.len() as TypeId {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(id)
                && name == &key.name
                && type_args == &key.type_args
            {
                self.type_substitutions.insert(id, concrete_type_id);
                self.type_to_mangled_name
                    .insert(id, self.instantiated.get(key).cloned().unwrap_or_default());
            }
        }

        Some(concrete)
    }

    /// Substitute type parameters in a type with concrete types
    fn substitute_type(
        &self,
        type_id: TypeId,
        substitution: &HashMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::TypeParam { index, .. } => {
                // Direct substitution
                *substitution.get(&index).unwrap_or(&type_id)
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type(elem, substitution, type_table);
                type_table.intern(ResolvedType::BuiltinArray(new_elem))
            }
            ResolvedType::Option(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_option(new_inner)
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_mut_ref(new_inner)
            }
            ResolvedType::Tuple(elems) => {
                let new_elems: Vec<TypeId> = elems
                    .iter()
                    .map(|&e| self.substitute_type(e, substitution, type_table))
                    .collect();
                type_table.make_tuple(new_elems)
            }
            ResolvedType::Result { ok, err } => {
                let new_ok = self.substitute_type(ok, substitution, type_table);
                let new_err = self.substitute_type(err, substitution, type_table);
                type_table.make_result(new_ok, new_err)
            }
            ResolvedType::GenericInstance {
                name,
                module_path,
                type_args,
            } => {
                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute_type(arg, substitution, type_table))
                    .collect();

                // Check if there's already a monomorphized struct for this instance
                // Build the mangled name: Counter$i32 (using type names)
                let mut mangled_name = name.clone();
                for &arg in &new_args {
                    mangled_name.push('$');
                    mangled_name.push_str(&self.type_id_to_name(arg, type_table));
                }

                // Look for existing struct with this name
                for tid in 0..type_table.len() as u32 {
                    if let ResolvedType::Struct {
                        name: struct_name, ..
                    } = type_table.get(tid)
                        && struct_name == &mangled_name
                    {
                        return tid;
                    }
                }

                // Fallback to GenericInstance if no monomorphized struct found
                type_table.make_generic_instance(name, module_path, new_args)
            }
            // Other types don't contain type parameters
            _ => type_id,
        }
    }

    // ========================================================================
    // Function Monomorphization
    // ========================================================================

    /// Collect function instantiation sites from Call/MethodCall/StaticCall expressions
    fn collect_function_instantiation_sites(
        &mut self,
        module: &TirModule,
        generic_functions: &HashMap<String, TirFunction>,
    ) {
        for func in &module.functions {
            if let Some(body) = &func.body {
                self.collect_func_instantiation_sites_in_block(
                    body,
                    generic_functions,
                    &module.type_table,
                );
            }
        }
    }

    fn collect_func_instantiation_sites_in_block(
        &mut self,
        block: &TirBlock,
        generic_functions: &HashMap<String, TirFunction>,
        type_table: &TypeTable,
    ) {
        for stmt in &block.stmts {
            self.collect_func_instantiation_sites_in_stmt(stmt, generic_functions, type_table);
        }
    }

    fn collect_func_instantiation_sites_in_stmt(
        &mut self,
        stmt: &TirStmt,
        generic_functions: &HashMap<String, TirFunction>,
        type_table: &TypeTable,
    ) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.collect_func_instantiation_sites_in_expr(
                        expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    condition,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(
                    then_block,
                    generic_functions,
                    type_table,
                );
                if let Some(else_blk) = else_block {
                    self.collect_func_instantiation_sites_in_block(
                        else_blk,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirStmtKind::While { condition, body } => {
                self.collect_func_instantiation_sites_in_expr(
                    condition,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(body, generic_functions, type_table);
            }
            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                if let Some(cond) = condition {
                    self.collect_func_instantiation_sites_in_expr(
                        cond,
                        generic_functions,
                        type_table,
                    );
                }
                self.collect_func_instantiation_sites_in_block(body, generic_functions, type_table);
                if let Some(upd) = update {
                    self.collect_func_instantiation_sites_in_expr(
                        upd,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirStmtKind::Loop { body } => {
                self.collect_func_instantiation_sites_in_block(body, generic_functions, type_table);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.collect_func_instantiation_sites_in_expr(
                    iterable,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(body, generic_functions, type_table);
            }
            TirStmtKind::Break | TirStmtKind::Continue | TirStmtKind::Assert { .. } => {}
        }
    }

    fn collect_func_instantiation_sites_in_expr(
        &mut self,
        expr: &TirExpr,
        generic_functions: &HashMap<String, TirFunction>,
        type_table: &TypeTable,
    ) {
        match &expr.kind {
            TirExprKind::Call {
                func_name,
                type_args,
                args,
                ..
            } => {
                // Check if this is a call to a generic function with explicit type args
                if !type_args.is_empty() && generic_functions.contains_key(func_name) {
                    let key = InstantiationKey {
                        name: func_name.clone(),
                        type_args: type_args.clone(),
                    };
                    if !self.function_instantiated.contains_key(&key) {
                        let mangled = self.mangle_function_name(&key);
                        self.function_instantiated.insert(key.clone(), mangled);
                        self.function_pending.push(key);
                    }
                }
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        arg,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::MethodCall {
                receiver,
                method_name,
                type_args,
                args,
            } => {
                // Check if this is a method call with explicit type args
                if !type_args.is_empty() {
                    // Get the struct name from the receiver type
                    if let Some(struct_name) =
                        self.get_struct_name_from_type(receiver.type_id, type_table)
                    {
                        // Construct the mangled method name: Struct::method
                        let full_method_name = format!("{}::{}", struct_name, method_name);
                        if generic_functions.contains_key(&full_method_name) {
                            let key = InstantiationKey {
                                name: full_method_name,
                                type_args: type_args.clone(),
                            };
                            if !self.function_instantiated.contains_key(&key) {
                                let mangled = self.mangle_function_name(&key);
                                self.function_instantiated.insert(key.clone(), mangled);
                                self.function_pending.push(key);
                            }
                        }
                        // Handle "double generics": method call with type_args on a monomorphized generic struct
                        // e.g., c.transform::<i64>(100) where c: Container$i32 and transform<U>
                        else if struct_name.contains('$')
                            && let Some(dollar_pos) = struct_name.find('$')
                        {
                            let base_struct = &struct_name[..dollar_pos];
                            let impl_type_args_str = &struct_name[dollar_pos + 1..];

                            // Look for generic method: BaseStruct::method
                            let generic_method_name = format!("{}::{}", base_struct, method_name);
                            if let Some(generic_func) = generic_functions.get(&generic_method_name)
                            {
                                // Parse impl type args from struct name
                                let impl_type_args: Vec<TypeId> = impl_type_args_str
                                    .split('$')
                                    .filter_map(|type_name| {
                                        self.lookup_type_by_name(type_name, type_table)
                                    })
                                    .collect();

                                // Verify we have the right number of impl type args
                                if impl_type_args.len() == generic_func.impl_type_params.len() {
                                    // Combine impl type args with method type args
                                    let mut combined_type_args = impl_type_args;
                                    combined_type_args.extend(type_args.iter().copied());

                                    let key = InstantiationKey {
                                        name: generic_method_name,
                                        type_args: combined_type_args,
                                    };
                                    if !self.function_instantiated.contains_key(&key) {
                                        let mangled = self.mangle_method_name(
                                            &key,
                                            type_table,
                                            generic_func.impl_type_params.len(),
                                        );
                                        self.function_instantiated.insert(key.clone(), mangled);
                                        self.function_pending.push(key);
                                    }
                                }
                            }
                        }
                    }
                }

                // Also check if the receiver is a monomorphized generic struct
                // e.g., c.get() where c: Counter$i32
                if let Some(struct_name) =
                    self.get_struct_name_from_type(receiver.type_id, type_table)
                    && struct_name.contains('$')
                {
                    // Parse the base struct name and type args
                    if let Some(dollar_pos) = struct_name.find('$') {
                        let base_struct = &struct_name[..dollar_pos];
                        let type_args_str = &struct_name[dollar_pos + 1..];

                        // Look for generic method: BaseStruct::method
                        let generic_method_name = format!("{}::{}", base_struct, method_name);
                        if let Some(generic_func) = generic_functions.get(&generic_method_name) {
                            // Parse type args from mangled struct name
                            let impl_type_args: Vec<TypeId> = type_args_str
                                .split('$')
                                .filter_map(|type_name| {
                                    self.lookup_type_by_name(type_name, type_table)
                                })
                                .collect();

                            // Only queue if we have the right number of type args
                            if impl_type_args.len() == generic_func.impl_type_params.len() {
                                let key = InstantiationKey {
                                    name: generic_method_name,
                                    type_args: impl_type_args,
                                };
                                if !self.function_instantiated.contains_key(&key) {
                                    let mangled = self.mangle_method_name(
                                        &key,
                                        type_table,
                                        generic_func.impl_type_params.len(),
                                    );
                                    self.function_instantiated.insert(key.clone(), mangled);
                                    self.function_pending.push(key);
                                }
                            }
                        }
                    }
                }

                self.collect_func_instantiation_sites_in_expr(
                    receiver,
                    generic_functions,
                    type_table,
                );
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        arg,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::EffectCall { args, .. } => {
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        arg,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::StaticCall {
                func_name, args, ..
            } => {
                // Check if this is a call to a method on a monomorphized struct
                // e.g., func_name = "Counter$i32::zero" or "Counter$i32::default_value"
                if let Some(sep_pos) = func_name.find("::") {
                    let struct_part = &func_name[..sep_pos];
                    let method_name = &func_name[sep_pos + 2..];

                    // Check if struct_part contains $ (monomorphized)
                    if let Some(dollar_pos) = struct_part.find('$') {
                        let base_struct = &struct_part[..dollar_pos];
                        let type_args_str = &struct_part[dollar_pos + 1..];

                        // Look for generic method: BaseStruct::method
                        let generic_method_name = format!("{}::{}", base_struct, method_name);
                        if let Some(generic_func) = generic_functions.get(&generic_method_name) {
                            // Parse type args from mangled name
                            // Type args are type names like "i32" from Counter$i32::zero
                            let type_args: Vec<TypeId> = type_args_str
                                .split('$')
                                .filter_map(|type_name| {
                                    self.lookup_type_by_name(type_name, type_table)
                                })
                                .collect();

                            // Only queue if we have the right number of type args
                            if type_args.len() == generic_func.impl_type_params.len() {
                                let key = InstantiationKey {
                                    name: generic_method_name,
                                    type_args,
                                };
                                if !self.function_instantiated.contains_key(&key) {
                                    let mangled = self.mangle_method_name(
                                        &key,
                                        type_table,
                                        generic_func.impl_type_params.len(),
                                    );
                                    self.function_instantiated.insert(key.clone(), mangled);
                                    self.function_pending.push(key);
                                }
                            }
                        }
                    }
                }
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        arg,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_func_instantiation_sites_in_expr(left, generic_functions, type_table);
                self.collect_func_instantiation_sites_in_expr(right, generic_functions, type_table);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.collect_func_instantiation_sites_in_expr(inner, generic_functions, type_table);
            }
            TirExprKind::Block(block) => {
                self.collect_func_instantiation_sites_in_block(
                    block,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    condition,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(
                    then_branch,
                    generic_functions,
                    type_table,
                );
                if let Some(else_blk) = else_branch {
                    self.collect_func_instantiation_sites_in_block(
                        else_blk,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_func_instantiation_sites_in_expr(
                        elem,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_func_instantiation_sites_in_expr(
                    target,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.collect_func_instantiation_sites_in_expr(inner, generic_functions, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.collect_func_instantiation_sites_in_expr(inner, generic_functions, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_func_instantiation_sites_in_expr(array, generic_functions, type_table);
                self.collect_func_instantiation_sites_in_expr(index, generic_functions, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    scrutinee,
                    generic_functions,
                    type_table,
                );
                for arm in arms {
                    self.collect_func_instantiation_sites_in_expr(
                        &arm.body,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.collect_func_instantiation_sites_in_expr(body, generic_functions, type_table);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_func_instantiation_sites_in_expr(
                        &field.value,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_func_instantiation_sites_in_expr(
                    callee,
                    generic_functions,
                    type_table,
                );
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        arg,
                        generic_functions,
                        type_table,
                    );
                }
            }
            // Literals and simple expressions
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::Capture { .. } => {}
        }
    }

    /// Get the struct name from a type_id, unwrapping references if needed
    /// For generic instances, returns the mangled name with type args (e.g., "Array$i32")
    fn get_struct_name_from_type(&self, type_id: TypeId, type_table: &TypeTable) -> Option<String> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. } => Some(name.clone()),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Return the mangled name with type args (e.g., "Array$i32", "Box$String")
                if type_args.is_empty() {
                    Some(name.clone())
                } else {
                    let mut result = name.clone();
                    for arg in type_args {
                        result.push('$');
                        result.push_str(&self.type_id_to_name_component(*arg, type_table));
                    }
                    Some(result)
                }
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_name_from_type(*inner, type_table)
            }
            _ => None,
        }
    }

    /// Look up a TypeId by type name (e.g., "i32" -> TypeId for i32)
    fn lookup_type_by_name(&self, name: &str, type_table: &TypeTable) -> Option<TypeId> {
        // First check primitive types
        match name {
            "i8" => return Some(TypeTable::I8),
            "i16" => return Some(TypeTable::I16),
            "i32" => return Some(TypeTable::I32),
            "i64" => return Some(TypeTable::I64),
            "i128" => return Some(TypeTable::I128),
            "u8" => return Some(TypeTable::U8),
            "u16" => return Some(TypeTable::U16),
            "u32" => return Some(TypeTable::U32),
            "u64" => return Some(TypeTable::U64),
            "u128" => return Some(TypeTable::U128),
            "f32" => return Some(TypeTable::F32),
            "f64" => return Some(TypeTable::F64),
            "bool" => return Some(TypeTable::BOOL),
            "char" => return Some(TypeTable::CHAR),
            _ => {}
        }

        // Search for struct/generic instance by name
        for type_id in 0..type_table.len() as u32 {
            match type_table.get(type_id) {
                ResolvedType::Struct {
                    name: struct_name, ..
                } if struct_name == name => {
                    return Some(type_id);
                }
                ResolvedType::GenericInstance {
                    name: inst_name, ..
                } if inst_name == name => {
                    return Some(type_id);
                }
                _ => {}
            }
        }

        None
    }

    /// Generate mangled name for function instantiation: identity<i32> -> identity$i32
    fn mangle_function_name(&self, key: &InstantiationKey) -> String {
        let mut name = key.name.clone();
        for type_arg in &key.type_args {
            name.push('$');
            // Use TypeId directly for now - we'll improve this later
            name.push_str(&type_arg.to_string());
        }
        name
    }

    /// Generate mangled name for method instantiation
    /// Format: StructWithImplArgs::methodWithMethodArgs
    /// e.g., Container::transform with [i32, i64] and impl_type_params_count=1 -> Container$i32::transform$i64
    fn mangle_method_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params_count: usize,
    ) -> String {
        // key.name is like "Container::transform"
        if let Some(sep_pos) = key.name.find("::") {
            let struct_name = &key.name[..sep_pos];
            let method_name = &key.name[sep_pos + 2..];

            // Split type_args into impl args and method args
            let (impl_args, method_args) = key
                .type_args
                .split_at(std::cmp::min(impl_type_params_count, key.type_args.len()));

            // Build mangled struct name: Container$i32 (using impl type args)
            let mut mangled_struct = struct_name.to_string();
            for type_arg in impl_args {
                mangled_struct.push('$');
                mangled_struct.push_str(&self.type_id_to_name(*type_arg, type_table));
            }

            // Build mangled method name: transform$i64 (using method type args)
            let mut mangled_method = method_name.to_string();
            for type_arg in method_args {
                mangled_method.push('$');
                mangled_method.push_str(&self.type_id_to_name(*type_arg, type_table));
            }

            format!("{}::{}", mangled_struct, mangled_method)
        } else {
            // Fallback to regular function mangling
            self.mangle_function_name(key)
        }
    }

    /// Convert a TypeId to its string representation for name mangling
    fn type_id_to_name(&self, type_id: TypeId, type_table: &TypeTable) -> String {
        match type_table.get(type_id) {
            ResolvedType::Primitive(p) => format!("{:?}", p).to_lowercase(),
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let mut result = name.clone();
                result.push('$');
                result.push_str(
                    &type_args
                        .iter()
                        .map(|&t| self.type_id_to_name(t, type_table))
                        .collect::<Vec<_>>()
                        .join("$"),
                );
                result
            }
            _ => type_id.to_string(),
        }
    }

    /// Instantiate a generic function with concrete type arguments
    fn instantiate_function(
        &mut self,
        generic: &TirFunction,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirFunction> {
        let mangled_name = self.function_instantiated.get(key)?.clone();

        // Build substitution map: type param index -> concrete type
        // Include both method-level type params AND impl block type params
        let mut substitution: HashMap<u32, TypeId> = HashMap::new();

        // Add impl block type params first (e.g., T from impl Counter<T>)
        for (param, &arg) in generic.impl_type_params.iter().zip(key.type_args.iter()) {
            substitution.insert(param.index, arg);
        }

        // Add method-level type params (offset by impl type params count)
        // The method's type_params have their own indices (0, 1, ...) but in the type table,
        // method type params are offset by impl type params count.
        // e.g., impl<T> { fn foo<U>() } - T has index 0, U has index 1 in type table
        let offset = generic.impl_type_params.len() as u32;
        for (i, (param, &arg)) in generic
            .type_params
            .iter()
            .zip(key.type_args.iter().skip(offset as usize))
            .enumerate()
        {
            // Use offset + param.index to get the correct index in the type table
            substitution.insert(offset + param.index, arg);
            let _ = i; // suppress unused warning
        }

        // Substitute types in parameters
        let params: Vec<TirParam> = generic
            .params
            .iter()
            .map(|param| TirParam {
                name: param.name.clone(),
                type_id: self.substitute_type(param.type_id, &substitution, type_table),
                local_index: param.local_index,
                span: param.span,
            })
            .collect();

        // Substitute return type
        let return_type = self.substitute_type(generic.return_type, &substitution, type_table);

        // Substitute types in local_types
        let local_types: Vec<TypeId> = generic
            .local_types
            .iter()
            .map(|&t| self.substitute_type(t, &substitution, type_table))
            .collect();

        // Clone and substitute types in body
        let body = generic.body.as_ref().map(|b| {
            let mut new_body = b.clone();
            self.substitute_types_in_block(&mut new_body, &substitution, type_table);
            new_body
        });

        Some(TirFunction {
            name: mangled_name,
            is_pub: generic.is_pub,
            type_params: vec![],      // Concrete function has no type params
            impl_type_params: vec![], // Already monomorphized, no impl type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                type_args: key.type_args.clone(),
            }),
            params,
            return_type,
            effects: generic.effects.clone(),
            body,
            span: generic.span,
            local_count: generic.local_count,
            local_types,
            address_taken_locals: generic.address_taken_locals.clone(),
        })
    }

    /// Substitute type parameters in a block
    fn substitute_types_in_block(
        &self,
        block: &mut TirBlock,
        substitution: &HashMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        for stmt in &mut block.stmts {
            self.substitute_types_in_stmt(stmt, substitution, type_table);
        }
    }

    fn substitute_types_in_stmt(
        &self,
        stmt: &mut TirStmt,
        substitution: &HashMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                *type_id = self.substitute_type(*type_id, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.substitute_types_in_expr(expr, substitution, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.substitute_types_in_expr(expr, substitution, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.substitute_types_in_expr(condition, substitution, type_table);
                self.substitute_types_in_block(then_block, substitution, type_table);
                if let Some(else_blk) = else_block {
                    self.substitute_types_in_block(else_blk, substitution, type_table);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.substitute_types_in_expr(condition, substitution, type_table);
                self.substitute_types_in_block(body, substitution, type_table);
            }
            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                if let Some(cond) = condition {
                    self.substitute_types_in_expr(cond, substitution, type_table);
                }
                self.substitute_types_in_block(body, substitution, type_table);
                if let Some(upd) = update {
                    self.substitute_types_in_expr(upd, substitution, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.substitute_types_in_block(body, substitution, type_table);
            }
            TirStmtKind::ForOf {
                iterable,
                body,
                binding_type,
                ..
            } => {
                *binding_type = self.substitute_type(*binding_type, substitution, type_table);
                self.substitute_types_in_expr(iterable, substitution, type_table);
                self.substitute_types_in_block(body, substitution, type_table);
            }
            TirStmtKind::Break | TirStmtKind::Continue | TirStmtKind::Assert { .. } => {}
        }
    }

    fn substitute_types_in_expr(
        &self,
        expr: &mut TirExpr,
        substitution: &HashMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        // Substitute the expression's own type
        expr.type_id = self.substitute_type(expr.type_id, substitution, type_table);

        // Recurse into sub-expressions
        match &mut expr.kind {
            TirExprKind::Call {
                type_args, args, ..
            } => {
                // Substitute type args themselves
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                type_args,
                args,
                ..
            } => {
                self.substitute_types_in_expr(receiver, substitution, type_table);
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::EffectCall { args, .. } => {
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::StaticCall {
                func_name, args, ..
            } => {
                // Substitute type parameter names in func_name
                // e.g., "Box$T::new" with T->i32 becomes "Box$i32::new"
                let mut new_func_name = func_name.clone();
                for (&param_index, &concrete_type_id) in substitution {
                    // Find the type param name by looking up types with this index
                    for tid in 0..type_table.len() as u32 {
                        if let ResolvedType::TypeParam { name, index } = type_table.get(tid)
                            && *index == param_index
                        {
                            // Replace $T with $concrete_type_name
                            let concrete_name = self.type_id_to_name(concrete_type_id, type_table);
                            new_func_name = new_func_name
                                .replace(&format!("${}", name), &format!("${}", concrete_name));
                            break;
                        }
                    }
                }
                *func_name = new_func_name;
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.substitute_types_in_expr(left, substitution, type_table);
                self.substitute_types_in_expr(right, substitution, type_table);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.substitute_types_in_expr(inner, substitution, type_table);
            }
            TirExprKind::Block(block) => {
                self.substitute_types_in_block(block, substitution, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.substitute_types_in_expr(condition, substitution, type_table);
                self.substitute_types_in_block(then_branch, substitution, type_table);
                if let Some(else_blk) = else_branch {
                    self.substitute_types_in_block(else_blk, substitution, type_table);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.substitute_types_in_expr(elem, substitution, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.substitute_types_in_expr(target, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                *target_type = self.substitute_type(*target_type, substitution, type_table);
                self.substitute_types_in_expr(inner, substitution, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.substitute_types_in_expr(inner, substitution, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.substitute_types_in_expr(array, substitution, type_table);
                self.substitute_types_in_expr(index, substitution, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.substitute_types_in_expr(scrutinee, substitution, type_table);
                for arm in arms {
                    self.substitute_types_in_expr(&mut arm.body, substitution, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.substitute_types_in_expr(body, substitution, type_table);
            }
            TirExprKind::StructLiteral {
                struct_type,
                fields,
                ..
            } => {
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);
                for field in fields {
                    self.substitute_types_in_expr(&mut field.value, substitution, type_table);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.substitute_types_in_expr(callee, substitution, type_table);
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            // Literals and other simple expressions
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::Capture { .. } => {}
        }
    }

    /// Rewrite function calls in all functions to use monomorphized names
    fn rewrite_function_calls_in_module(&self, module: &mut TirModule) {
        for func in &mut module.functions {
            if let Some(body) = &mut func.body {
                self.rewrite_function_calls_in_block(body, &module.type_table);
                // Sync local_types with Let statement types
                Self::sync_local_types_from_lets(body, &mut func.local_types);
                // Update all Local expression types based on local_types
                Self::update_local_expr_types(body, &func.local_types);
            }
        }
    }

    /// Sync local_types array from Let statements that may have been updated
    fn sync_local_types_from_lets(block: &TirBlock, local_types: &mut [TypeId]) {
        for stmt in &block.stmts {
            match &stmt.kind {
                TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } => {
                    if let Some(local_type) = local_types.get_mut(*local_index as usize) {
                        *local_type = *type_id;
                    }
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    Self::sync_local_types_from_lets(then_block, local_types);
                    if let Some(else_blk) = else_block {
                        Self::sync_local_types_from_lets(else_blk, local_types);
                    }
                }
                TirStmtKind::While { body, .. }
                | TirStmtKind::Loop { body }
                | TirStmtKind::For { body, .. }
                | TirStmtKind::ForOf { body, .. } => {
                    Self::sync_local_types_from_lets(body, local_types);
                }
                _ => {}
            }
        }
    }

    /// Update all Local expression types based on local_types array
    fn update_local_expr_types(block: &mut TirBlock, local_types: &[TypeId]) {
        for stmt in &mut block.stmts {
            Self::update_local_expr_types_in_stmt(stmt, local_types);
        }
    }

    fn update_local_expr_types_in_stmt(stmt: &mut TirStmt, local_types: &[TypeId]) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                Self::update_local_expr_types_in_expr(value, local_types);
            }
            TirStmtKind::Expr(expr) => {
                Self::update_local_expr_types_in_expr(expr, local_types);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    Self::update_local_expr_types_in_expr(expr, local_types);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::update_local_expr_types_in_expr(condition, local_types);
                Self::update_local_expr_types(then_block, local_types);
                if let Some(else_blk) = else_block {
                    Self::update_local_expr_types(else_blk, local_types);
                }
            }
            TirStmtKind::While { condition, body } => {
                Self::update_local_expr_types_in_expr(condition, local_types);
                Self::update_local_expr_types(body, local_types);
            }
            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                if let Some(expr) = condition {
                    Self::update_local_expr_types_in_expr(expr, local_types);
                }
                Self::update_local_expr_types(body, local_types);
                if let Some(expr) = update {
                    Self::update_local_expr_types_in_expr(expr, local_types);
                }
            }
            TirStmtKind::Loop { body } => {
                Self::update_local_expr_types(body, local_types);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                Self::update_local_expr_types_in_expr(iterable, local_types);
                Self::update_local_expr_types(body, local_types);
            }
            _ => {}
        }
    }

    fn update_local_expr_types_in_expr(expr: &mut TirExpr, local_types: &[TypeId]) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } => {
                if let Some(&local_type) = local_types.get(*index as usize) {
                    expr.type_id = local_type;
                }
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::update_local_expr_types_in_expr(receiver, local_types);
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::update_local_expr_types_in_expr(left, local_types);
                Self::update_local_expr_types_in_expr(right, local_types);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::update_local_expr_types_in_expr(inner, local_types);
            }
            TirExprKind::Block(block) => {
                Self::update_local_expr_types(block, local_types);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::update_local_expr_types_in_expr(condition, local_types);
                Self::update_local_expr_types(then_branch, local_types);
                if let Some(else_blk) = else_branch {
                    Self::update_local_expr_types(else_blk, local_types);
                }
            }
            TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::update_local_expr_types_in_expr(elem, local_types);
                }
            }
            TirExprKind::Index { expr, index } => {
                Self::update_local_expr_types_in_expr(expr, local_types);
                Self::update_local_expr_types_in_expr(index, local_types);
            }
            TirExprKind::Assign { target, value } => {
                Self::update_local_expr_types_in_expr(target, local_types);
                Self::update_local_expr_types_in_expr(value, local_types);
            }
            TirExprKind::Match { expr, arms } => {
                Self::update_local_expr_types_in_expr(expr, local_types);
                for arm in arms {
                    Self::update_local_expr_types_in_expr(&mut arm.body, local_types);
                }
            }
            TirExprKind::Closure { .. } => {
                // Closures have their own local scope, don't update with parent's local_types
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::update_local_expr_types_in_expr(&mut field.value, local_types);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::update_local_expr_types_in_expr(callee, local_types);
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            _ => {}
        }
    }

    fn rewrite_function_calls_in_block(&self, block: &mut TirBlock, type_table: &TypeTable) {
        for stmt in &mut block.stmts {
            self.rewrite_function_calls_in_stmt(stmt, type_table);
        }
    }

    fn rewrite_function_calls_in_stmt(&self, stmt: &mut TirStmt, type_table: &TypeTable) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                self.rewrite_function_calls_in_expr(value, type_table);
                // Update the Let's type_id if it was a type parameter that got substituted
                // The value's type_id may have been updated during rewriting
                if type_table.contains_type_param(*type_id)
                    && !type_table.contains_type_param(value.type_id)
                {
                    *type_id = value.type_id;
                }
            }
            TirStmtKind::Expr(expr) => {
                self.rewrite_function_calls_in_expr(expr, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.rewrite_function_calls_in_expr(expr, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.rewrite_function_calls_in_expr(condition, type_table);
                self.rewrite_function_calls_in_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.rewrite_function_calls_in_block(else_blk, type_table);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.rewrite_function_calls_in_expr(condition, type_table);
                self.rewrite_function_calls_in_block(body, type_table);
            }
            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                if let Some(cond) = condition {
                    self.rewrite_function_calls_in_expr(cond, type_table);
                }
                self.rewrite_function_calls_in_block(body, type_table);
                if let Some(upd) = update {
                    self.rewrite_function_calls_in_expr(upd, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.rewrite_function_calls_in_block(body, type_table);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.rewrite_function_calls_in_expr(iterable, type_table);
                self.rewrite_function_calls_in_block(body, type_table);
            }
            TirStmtKind::Break | TirStmtKind::Continue | TirStmtKind::Assert { .. } => {}
        }
    }

    fn rewrite_function_calls_in_expr(&self, expr: &mut TirExpr, type_table: &TypeTable) {
        match &mut expr.kind {
            TirExprKind::Call {
                func_name,
                type_args,
                args,
                ..
            } => {
                // If this is a generic call, rewrite to monomorphized name
                if !type_args.is_empty() {
                    let key = InstantiationKey {
                        name: func_name.clone(),
                        type_args: type_args.clone(),
                    };
                    if let Some(mangled) = self.function_instantiated.get(&key) {
                        *func_name = mangled.clone();
                        type_args.clear(); // Clear type args - now using concrete function
                    }
                }
                for arg in args {
                    self.rewrite_function_calls_in_expr(arg, type_table);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                method_name,
                type_args,
                args,
            } => {
                // If this is a generic method call, rewrite to monomorphized name
                if !type_args.is_empty()
                    && let Some(struct_name) =
                        self.get_struct_name_from_type(receiver.type_id, type_table)
                {
                    let full_method_name = format!("{}::{}", struct_name, method_name);
                    let key = InstantiationKey {
                        name: full_method_name.clone(),
                        type_args: type_args.clone(),
                    };
                    if let Some(mangled) = self.function_instantiated.get(&key) {
                        // Update method_name to the monomorphized name
                        // We need to extract just the method part from the mangled name
                        // e.g., "Point::transform$2" -> we keep the full name for codegen
                        // Actually, codegen constructs the full name, so we just need to update
                        // the method_name part. The mangled name is "Struct::method$types",
                        // so we extract the part after "::"
                        if let Some(method_part) = mangled.split("::").nth(1) {
                            *method_name = method_part.to_string();
                        }
                        type_args.clear(); // Clear type args - now using concrete method
                    }
                    // Handle "double generics": method on monomorphized generic struct
                    // e.g., c.transform::<i64>() where c: Container$i32
                    else if struct_name.contains('$')
                        && let Some(dollar_pos) = struct_name.find('$')
                    {
                        let base_struct = &struct_name[..dollar_pos];
                        let impl_type_args_str = &struct_name[dollar_pos + 1..];

                        // Parse impl type args from struct name
                        let impl_type_args: Vec<TypeId> = impl_type_args_str
                            .split('$')
                            .filter_map(|type_name| self.lookup_type_by_name(type_name, type_table))
                            .collect();

                        // Combine impl type args with method type args
                        let mut combined_type_args = impl_type_args.clone();
                        combined_type_args.extend(type_args.iter().copied());

                        // Look up with base struct name and combined type args
                        let generic_method_name = format!("{}::{}", base_struct, method_name);
                        let combined_key = InstantiationKey {
                            name: generic_method_name,
                            type_args: combined_type_args.clone(),
                        };
                        if let Some(mangled) = self.function_instantiated.get(&combined_key) {
                            if let Some(method_part) = mangled.split("::").nth(1) {
                                *method_name = method_part.to_string();
                            }
                            type_args.clear();

                            // Also update the expression's type_id if it's a type parameter
                            // Build substitution map: impl args + method args
                            if let ResolvedType::TypeParam { index, .. } =
                                type_table.get(expr.type_id)
                            {
                                let impl_count = impl_type_args.len() as u32;
                                if *index < impl_count {
                                    // It's an impl type param
                                    if let Some(&concrete) = combined_type_args.get(*index as usize)
                                    {
                                        expr.type_id = concrete;
                                    }
                                } else {
                                    // It's a method type param (offset by impl_count)
                                    let method_index = *index - impl_count;
                                    if let Some(&concrete) =
                                        combined_type_args.get((impl_count + method_index) as usize)
                                    {
                                        expr.type_id = concrete;
                                    }
                                }
                            }
                        }
                    }
                }
                self.rewrite_function_calls_in_expr(receiver, type_table);
                for arg in args {
                    self.rewrite_function_calls_in_expr(arg, type_table);
                }
            }
            TirExprKind::EffectCall { args, .. } | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.rewrite_function_calls_in_expr(arg, type_table);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.rewrite_function_calls_in_expr(left, type_table);
                self.rewrite_function_calls_in_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.rewrite_function_calls_in_expr(inner, type_table);
            }
            TirExprKind::Block(block) => {
                self.rewrite_function_calls_in_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.rewrite_function_calls_in_expr(condition, type_table);
                self.rewrite_function_calls_in_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.rewrite_function_calls_in_block(else_blk, type_table);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.rewrite_function_calls_in_expr(elem, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.rewrite_function_calls_in_expr(target, type_table);
                self.rewrite_function_calls_in_expr(value, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.rewrite_function_calls_in_expr(inner, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.rewrite_function_calls_in_expr(inner, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.rewrite_function_calls_in_expr(array, type_table);
                self.rewrite_function_calls_in_expr(index, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.rewrite_function_calls_in_expr(scrutinee, type_table);
                for arm in arms {
                    self.rewrite_function_calls_in_expr(&mut arm.body, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.rewrite_function_calls_in_expr(body, type_table);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.rewrite_function_calls_in_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.rewrite_function_calls_in_expr(callee, type_table);
                for arg in args {
                    self.rewrite_function_calls_in_expr(arg, type_table);
                }
            }
            // Literals and simple expressions
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::Capture { .. } => {}
        }
    }
}

// ============================================================================
// String Literal Collection
// ============================================================================

/// Collects all string literals from a TIR module for the data section
struct StringCollector {
    strings: Vec<String>,
}

impl StringCollector {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
        }
    }

    fn into_strings(self) -> Vec<String> {
        self.strings
    }

    fn add_string(&mut self, s: String) {
        if !self.strings.contains(&s) {
            self.strings.push(s);
        }
    }

    fn collect_module(&mut self, module: &TirModule) {
        for func in &module.functions {
            if let Some(body) = &func.body {
                self.collect_block(body);
            }
        }
    }

    fn collect_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.collect_stmt(stmt);
        }
    }

    fn collect_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.collect_expr(expr);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.collect_expr(expr);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_block(else_blk);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.collect_expr(condition);
                self.collect_block(body);
            }
            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                if let Some(cond) = condition {
                    self.collect_expr(cond);
                }
                self.collect_block(body);
                if let Some(upd) = update {
                    self.collect_expr(upd);
                }
            }
            TirStmtKind::Loop { body } => {
                self.collect_block(body);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.collect_expr(iterable);
                self.collect_block(body);
            }
            TirStmtKind::Break | TirStmtKind::Continue => {}
            TirStmtKind::Assert {
                condition,
                condition_source,
                message,
                intermediates,
            } => {
                self.collect_expr(condition);
                if let Some(msg) = message {
                    self.collect_expr(msg);
                }
                for (_, expr, _) in intermediates {
                    self.collect_expr(expr);
                }
                // Collect static strings used in assert messages
                self.add_string("Assertion failed:\n".to_string());
                self.add_string("Assertion failed: ".to_string());
                self.add_string(format!("condition: {}\n", condition_source));
                self.add_string("\n".to_string());
                for (name, _, _) in intermediates {
                    self.add_string(format!("{}: ", name));
                }
            }
        }
    }

    fn collect_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::StringLiteral(s) => {
                self.add_string(s.clone());
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_expr(left);
                self.collect_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.collect_expr(receiver);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::Block(block) => {
                self.collect_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.collect_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_expr(&field.value);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_expr(elem);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_expr(target);
                self.collect_expr(value);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_expr(array);
                self.collect_expr(index);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_expr(scrutinee);
                for arm in arms {
                    self.collect_expr(&arm.body);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.collect_expr(body);
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_expr(callee);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            // Literals and simple expressions don't contain strings
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::Capture { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lower_passthrough() {
        let module = TirModule::new(vec!["test".to_string()]);
        let lowered = lower(module);
        assert_eq!(lowered.path, vec!["test".to_string()]);
    }

    #[test]
    fn test_string_collector_empty() {
        let module = TirModule::new(vec!["test".to_string()]);
        let lowered = lower(module);
        assert!(lowered.string_literals.is_empty());
    }
}
