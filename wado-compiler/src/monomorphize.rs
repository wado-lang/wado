//! Monomorphization pass for Wado TIR
//!
//! This phase instantiates generic structs and functions with concrete types.
//! Monomorphization is a separate compilation phase that runs after type resolution
//! and before the lower phase.
//!
//! The monomorphization process:
//! 1. Collect all generic struct and function definitions
//! 2. Find instantiation sites (`GenericInstance` types, generic function calls)
//! 3. Generate concrete struct and function definitions
//! 4. Rewrite types and function calls to use monomorphized names

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::name::{
    FreeFunctionName, LocalMethodName, MethodName, ModuleSource, mangle_generic_name,
};

/// Returns the key used to store/look up a generic function in the global function map.
///
/// Methods use their unqualified name — the struct name already provides namespace.
/// Free functions are module-qualified to keep same-named generics from different
/// modules distinct (e.g., `wrap<T>` in `mod_a` vs `mod_b`).
fn generic_function_key(is_method: bool, module_source: &ModuleSource, name: &str) -> String {
    if is_method {
        name.to_string()
    } else {
        FreeFunctionName::from_module_source(module_source, name).to_string()
    }
}
use crate::project::Project;
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBinaryOp, TirBlock,
    TirExpr, TirExprKind, TirField, TirFunction, TirModule, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirTemplatePart, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Monomorphize a single TIR module
///
/// This performs monomorphization of generic types and functions
/// within a single module without cross-module generic function support.
pub fn monomorphize_module(module: TirModule) -> TirModule {
    let module_source = module.module_source.clone();
    let mut monomorph = Monomorphizer::new(module_source);
    monomorph.monomorphize_with_externals(
        module,
        &IndexMap::default(),
        &IndexMap::default(),
        &IndexMap::default(),
    )
}

/// Monomorphize a Project (Project -> Project)
///
/// This is the main entry point for the monomorphize phase. It monomorphizes all TIR modules
/// in the project with cross-module generic function support.
pub fn monomorphize_project(mut project: Project) -> Project {
    project.tir_modules = monomorphize_modules_indexed(project.tir_modules);

    // Strip effect params from all functions. Effect params have been validated by the
    // effect checker (which runs before monomorphization) and are not needed downstream.
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            func.effects.retain(|e| !e.is_param());
        }
    }

    project
}

/// Monomorphize multiple modules with cross-module generic function and struct support
///
/// This function enables monomorphization of generic functions and structs defined in one module
/// but used in another (e.g., Array methods from prelude, `TreeMap` from prelude used in user code).
///
/// IMPORTANT: Requires unified type tables - all modules must share the same `TypeTable`
/// so that `TypeIds` are valid across modules.
pub fn monomorphize_modules_indexed(
    modules: IndexMap<ModuleSource, TirModule>,
) -> IndexMap<ModuleSource, TirModule> {
    // First pass: collect all generic functions from all modules.
    let mut all_generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
    for (module_source, module) in &modules {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                let key = generic_function_key(func.is_method(), module_source, &func.name);
                all_generic_functions.insert(key, Rc::clone(func_rc));
            }
        }
    }

    // Collect all generic structs from all modules, tracking ALL source modules
    // (a struct name can appear in multiple modules due to shadowing)
    // This includes private structs as they may be needed for instantiating public structs
    // (e.g., TreeMap uses TreeMapNode internally)
    let mut all_generic_structs: IndexMap<String, Vec<(ModuleSource, TirStruct)>> =
        IndexMap::default();
    for (module_source, module) in &modules {
        for tir_struct in &module.structs {
            if !tir_struct.type_params.is_empty() {
                all_generic_structs
                    .entry(tir_struct.name.clone())
                    .or_default()
                    .push((module_source.clone(), tir_struct.clone()));
            }
        }
    }

    // Identify entry module and its generic struct names (for shadowing detection)
    // Entry module is the one with ModuleSource::EntryPoint or the last module (user's file)
    let entry_module_source = modules
        .keys()
        .find(|s| matches!(s, ModuleSource::EntryPoint { .. }))
        .cloned()
        .unwrap_or_else(|| {
            modules
                .keys()
                .last()
                .cloned()
                .expect("monomorphize_modules_indexed called with empty modules")
        });

    let entry_generic_struct_names: IndexSet<String> = modules
        .get(&entry_module_source)
        .map(|m| {
            m.structs
                .iter()
                .filter(|s| !s.type_params.is_empty())
                .map(|s| s.name.clone())
                .collect()
        })
        .unwrap_or_default();

    // Collect all concrete trait method functions from all modules.
    // Maps function name (e.g., "i32^Stringify::to_str") → module source.
    // This enables correct module resolution when monomorphizing type param
    // receiver calls (e.g., T^Trait::method → ConcreteType^Trait::method).
    let mut trait_method_locations: IndexMap<String, ModuleSource> = IndexMap::default();
    for (module_source, module) in &modules {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Only collect non-generic trait methods (concrete impls like "i32^Stringify::to_str")
            if !func.has_real_type_params()
                && func.impl_type_params.is_empty()
                && let Some(ref info) = func.method_info
                && info.trait_name.is_some()
            {
                trait_method_locations.insert(func.name.clone(), module_source.clone());
            }
        }
    }

    // Second pass: monomorphize each module using the combined generic functions and structs
    modules
        .into_iter()
        .map(|(module_source, module)| {
            (
                module_source.clone(),
                monomorphize_with_externals(
                    module,
                    &module_source,
                    &entry_module_source,
                    &entry_generic_struct_names,
                    &all_generic_functions,
                    &all_generic_structs,
                    &trait_method_locations,
                ),
            )
        })
        .collect()
}

/// Monomorphize a single module with access to cross-module generic functions and structs
fn monomorphize_with_externals(
    module: TirModule,
    current_module_source: &ModuleSource,
    entry_module_source: &ModuleSource,
    entry_generic_struct_names: &IndexSet<String>,
    all_generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    all_generic_structs_with_sources: &IndexMap<String, Vec<(ModuleSource, TirStruct)>>,
    trait_method_locations: &IndexMap<String, ModuleSource>,
) -> TirModule {
    let is_entry_module = current_module_source == entry_module_source;

    // Find modules whose structs are shadowed by the entry module's definitions
    // This is computed globally, not per-module, because we want consistent shadowing
    let mut shadowed_modules: IndexSet<ModuleSource> = IndexSet::default();
    for entry_struct_name in entry_generic_struct_names {
        if let Some(sources) = all_generic_structs_with_sources.get(entry_struct_name) {
            // Find external modules that define this struct (not the entry module)
            for (external_module_source, _) in sources {
                if external_module_source != entry_module_source {
                    shadowed_modules.insert(external_module_source.clone());
                }
            }
        }
    }

    // Build generic structs map based on whether this is the entry module or not
    let mut all_generic_structs: IndexMap<String, TirStruct> = IndexMap::default();

    if is_entry_module {
        // Entry module: use its own structs + non-shadowed external structs
        for (name, sources) in all_generic_structs_with_sources {
            let mut selected: Option<&TirStruct> = None;

            // First, try to find local definition (entry module's own struct)
            for (source, tir_struct) in sources {
                if source == entry_module_source {
                    selected = Some(tir_struct);
                    break;
                }
            }

            // If no local definition, try external (from non-shadowed modules)
            if selected.is_none() {
                for (source, tir_struct) in sources {
                    if !shadowed_modules.contains(source) {
                        selected = Some(tir_struct);
                        break;
                    }
                }
            }

            if let Some(tir_struct) = selected {
                all_generic_structs.insert(name.clone(), tir_struct.clone());
            }
        }
    } else {
        // Non-entry module: use structs from any non-shadowed module.
        // This enables cross-module monomorphization (e.g., `./treemap-mod.wado`
        // can instantiate `ArrayIter<TreeMapEntry<String,Value>>` from core:prelude).
        for (name, sources) in all_generic_structs_with_sources {
            // Skip if this struct name is defined in entry module (shadowed)
            if entry_generic_struct_names.contains(name) {
                continue;
            }

            // Prefer structs from the current module, fall back to any non-shadowed module
            let mut selected: Option<&TirStruct> = None;
            for (source, tir_struct) in sources {
                if source == current_module_source {
                    selected = Some(tir_struct);
                    break;
                }
            }
            if selected.is_none() {
                for (source, tir_struct) in sources {
                    if !shadowed_modules.contains(source) {
                        selected = Some(tir_struct);
                        break;
                    }
                }
            }
            if let Some(tir_struct) = selected {
                all_generic_structs.insert(name.clone(), tir_struct.clone());
            }
        }
    }

    let mut monomorph = Monomorphizer::new(current_module_source.clone());
    monomorph.monomorphize_with_externals(
        module,
        all_generic_functions,
        &all_generic_structs,
        trait_method_locations,
    )
}

/// Trait for mutable traversal of TIR trees.
///
/// Override `visit_*` methods to add custom logic at specific nodes.
/// Call the corresponding `walk_*` method within your override to recurse into children.
/// The default implementations simply delegate to `walk_*`.
trait TirMutVisitor {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.walk_expr(expr);
    }
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        self.walk_stmt(stmt);
    }
    fn visit_block(&mut self, block: &mut TirBlock) {
        self.walk_block(block);
    }
    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        self.walk_pattern(pattern);
    }

    fn walk_block(&mut self, block: &mut TirBlock) {
        for stmt in &mut block.stmts {
            self.visit_stmt(stmt);
        }
    }

    fn walk_stmt(&mut self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.visit_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.visit_expr(expr);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.visit_expr(expr);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_block);
                if let Some(else_blk) = else_block {
                    self.visit_block(else_blk);
                }
            }
            TirStmtKind::Loop { body } => {
                self.visit_block(body);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.visit_expr(scrutinee);
                self.visit_pattern(pattern);
                self.visit_block(then_block);
                if let Some(else_blk) = else_block {
                    self.visit_block(else_blk);
                }
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.visit_pattern(pattern);
                self.visit_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {}
            TirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.visit_expr(iterable);
                self.visit_block(body);
            }
        }
    }

    fn walk_pattern(&mut self, pattern: &mut TirPattern) {
        match pattern {
            TirPattern::Wildcard | TirPattern::Binding { .. } | TirPattern::Literal(_) => {}
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    self.visit_pattern(p);
                }
            }
            TirPattern::Variant { bindings, .. } => {
                for binding in bindings {
                    self.visit_pattern(binding);
                }
            }
            TirPattern::Enum { .. } => {}
            TirPattern::Struct { fields, .. } => {
                for field in fields {
                    self.visit_pattern(&mut field.pattern);
                }
            }
        }
    }

    fn walk_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
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
            TirExprKind::GlobalVarSet { value, .. } => {
                self.visit_expr(value);
            }
            TirExprKind::Binary { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
                self.visit_expr(inner);
            }
            TirExprKind::Assign { target, value }
            | TirExprKind::Index {
                expr: target,
                index: value,
            } => {
                self.visit_expr(target);
                self.visit_expr(value);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.visit_block(else_blk);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&mut arm.body);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.visit_expr(&mut field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.visit_expr(elem);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.visit_expr(body);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.visit_expr(payload_expr);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.visit_block(arm);
                }
                self.visit_block(default);
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.visit_expr(inner);
                    }
                }
            }
        }
    }
}

/// Tracks struct monomorphization state
struct StructInstState {
    /// Map from (`generic_name`, `type_args`) to mangled name
    instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending struct instantiations
    pending: Vec<InstantiationKey>,
    /// Map from `GenericInstance` `TypeId` to monomorphized Struct `TypeId`
    type_substitutions: IndexMap<TypeId, TypeId>,
    /// Map from `GenericInstance` `TypeId` to mangled struct name
    type_to_mangled_name: IndexMap<TypeId, String>,
    /// Reverse lookup: mangled struct name -> `InstantiationKey`
    mangled_to_key: IndexMap<String, InstantiationKey>,
}

/// Tracks function monomorphization state
struct FuncInstState {
    /// Map from (`generic_func_name`, `type_args`) to mangled function name
    instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending function instantiations
    pending: Vec<InstantiationKey>,
    /// Reverse lookup: mangled function name -> `InstantiationKey`
    mangled_to_key: IndexMap<String, InstantiationKey>,
    /// Map from concrete trait method function name → module where it's defined.
    /// Used to resolve the correct module when substituting type param receivers.
    trait_method_locations: IndexMap<String, ModuleSource>,
}

/// Monomorphizer collects generic instantiations and generates concrete types
struct Monomorphizer {
    /// The module source where monomorphized entities are being generated
    current_module_source: ModuleSource,
    structs: StructInstState,
    functions: FuncInstState,
}

impl Monomorphizer {
    fn new(current_module_source: ModuleSource) -> Self {
        Self {
            current_module_source,
            structs: StructInstState {
                instantiated: IndexMap::default(),
                pending: Vec::new(),
                type_substitutions: IndexMap::default(),
                type_to_mangled_name: IndexMap::default(),
                mangled_to_key: IndexMap::default(),
            },
            functions: FuncInstState {
                instantiated: IndexMap::default(),
                pending: Vec::new(),
                mangled_to_key: IndexMap::default(),
                trait_method_locations: IndexMap::default(),
            },
        }
    }

    /// Perform monomorphization on a module, optionally with access to external generic
    /// functions and structs from other modules (e.g., Array methods from prelude).
    ///
    /// IMPORTANT: Requires unified type tables - `TypeIds` in external generics
    /// must be valid in the module's `type_table`.
    fn monomorphize_with_externals(
        &mut self,
        mut module: TirModule,
        external_generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        external_generic_structs: &IndexMap<String, TirStruct>,
        trait_method_locations: &IndexMap<String, ModuleSource>,
    ) -> TirModule {
        self.functions
            .trait_method_locations
            .clone_from(trait_method_locations);

        // Phase 1: Collect all generic struct definitions
        // Include both local structs AND external generic structs from other modules
        let mut generic_structs: IndexMap<String, TirStruct> = external_generic_structs.clone();

        // Local generic structs override external ones (allows module-local specialization)
        // This handles the case where user defines their own TreeMap that shadows prelude's
        for tir_struct in &module.structs {
            if !tir_struct.type_params.is_empty() {
                generic_structs.insert(tir_struct.name.clone(), tir_struct.clone());
            }
        }

        // Store in module for later phases
        module.generic_structs.clone_from(&generic_structs);

        // Build set of valid struct names for collection
        let valid_struct_names: IndexSet<String> = generic_structs.keys().cloned().collect();

        // Phase 2-4: Collect and instantiate structs iteratively
        // This is done in a loop because instantiating a struct (like TreeMap<String,i32>)
        // may create new GenericInstance types in its fields (like BTreeNode<String,i32>)
        // that also need to be instantiated.
        let mut new_structs = Vec::new();
        loop {
            // Collect instantiation sites from current type table
            self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);

            // If no new structs to instantiate, we're done
            if self.structs.pending.is_empty() {
                break;
            }

            // Process all pending struct instantiations
            while let Some(key) = self.structs.pending.pop() {
                if let Some(generic_struct) = generic_structs.get(&key.name)
                    && let Some(concrete) = self.instantiate_struct(
                        generic_struct,
                        &key,
                        &mut module.type_table.borrow_mut(),
                    )
                {
                    new_structs.push(concrete);
                }
            }
        }

        // Add monomorphized structs to module
        module.structs.extend(new_structs);

        // Phase 5: Remove generic structs from the concrete struct list
        module
            .structs
            .retain(|s| s.type_params.is_empty() || s.monomorph_info.is_some());

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module);

        // Phase 7: Collect all generic function definitions
        // Include both local functions AND external generic functions from other modules
        let mut generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>> =
            external_generic_functions.clone();

        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                let key =
                    generic_function_key(func.is_method(), &self.current_module_source, &func.name);
                generic_functions.insert(key, Rc::clone(func_rc));
            }
        }

        // Store in module for later phases
        module.generic_functions.clone_from(&generic_functions);

        // Phase 8: Collect function instantiation sites from Call expressions
        self.collect_function_instantiation_sites(&module, &generic_functions);

        // Phase 9: Process function instantiations and generate concrete functions
        // Use iterative approach: each newly instantiated function may have method calls
        // that need to be instantiated too (e.g., a generic method calling another generic
        // method on self, like sort() -> sort_by())
        //
        // For transitive scanning, exclude bodyless functions (builtins like array_new,
        // array_set, etc.) which are codegen intrinsics and must not be re-monomorphized.
        let scannable_generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>> =
            generic_functions
                .iter()
                .filter(|(_, f)| f.borrow().body.is_some())
                .map(|(k, v)| (k.clone(), Rc::clone(v)))
                .collect();

        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        while let Some(key) = self.functions.pending.pop() {
            // Instantiate the function (needs mutable borrow)
            let concrete = {
                let generic_func = generic_functions.get(&key.name);
                if let Some(gf) = generic_func {
                    let gf_borrowed = gf.borrow();
                    self.instantiate_function(
                        &gf_borrowed,
                        &key,
                        &mut module.type_table.borrow_mut(),
                    )
                } else {
                    None
                }
            };

            if let Some(concrete) = concrete {
                // Collect instantiation sites from the newly created function body
                // This handles transitive monomorphization (e.g., a generic method calling
                // another generic method on self, like sort() -> sort_by())
                if let Some(body) = &concrete.body {
                    self.collect_func_instantiation_sites_in_block(
                        body,
                        &scannable_generic_functions,
                        &module.type_table.borrow(),
                    );
                }
                new_functions.push(Rc::new(RefCell::new(concrete)));
            }
        }

        // Phase 10: Add monomorphized functions to module
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        // Remove functions with type_params OR impl_type_params (unless monomorphized)
        // Effect-only params don't count as generic (they're erased at compile time).
        module.functions.retain(|f| {
            let func = f.borrow();
            (!func.has_real_type_params() && func.impl_type_params.is_empty())
                || func.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        // Phase 12.5: Desugar comparison operators on non-primitive types in non-generic functions.
        // (Generic functions are handled during substitute_types_in_expr, but non-generic
        // functions with variant/struct == never go through that path.)
        self.desugar_comparisons_in_module(&mut module);

        // Phase 13: Second pass of struct instantiation
        // Function monomorphization may have created new GenericInstance types
        // (e.g., BTreeNode<String,i32>) that weren't in the type table during Phase 2.
        // Collect and instantiate these now.
        self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);
        let mut second_pass_structs = Vec::new();
        while let Some(key) = self.structs.pending.pop() {
            if let Some(generic_struct) = generic_structs.get(&key.name)
                && let Some(concrete) = self.instantiate_struct(
                    generic_struct,
                    &key,
                    &mut module.type_table.borrow_mut(),
                )
            {
                second_pass_structs.push(concrete);
            }
        }
        module.structs.extend(second_pass_structs);
        // Rewrite types again for any new struct instantiations
        self.rewrite_types_in_module(&mut module);

        module
    }

    /// Rewrite all `GenericInstance` `type_ids` in expressions to use monomorphized struct types
    fn rewrite_types_in_module(&self, module: &mut TirModule) {
        let mut rewriter = TypeRewriter {
            mono: self,
            type_table: &mut module.type_table.borrow_mut(),
        };

        // Rewrite struct field types
        for strct in &mut module.structs {
            for field in &mut strct.fields {
                field.type_id = rewriter.rewrite_type_id(field.type_id);
            }
        }

        // Rewrite function signatures and bodies
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            for param in &mut func.params {
                param.type_id = rewriter.rewrite_type_id(param.type_id);
            }
            func.return_type = rewriter.rewrite_type_id(func.return_type);
            for local_type in &mut func.local_types {
                *local_type = rewriter.rewrite_type_id(*local_type);
            }
            if let Some(body) = &mut func.body {
                rewriter.visit_block(body);
            }
        }

        // Rewrite global variable initializers
        for global in &mut module.globals {
            global.ty = rewriter.rewrite_type_id(global.ty);
            rewriter.visit_expr(&mut global.initializer);
        }
    }

    /// Rewrite a single `type_id`: if it's a `GenericInstance`, return the concrete struct `type_id`.
    /// Also handles container types (Array, Option, Tuple) that may contain `GenericInstance`.
    fn rewrite_type_id(&self, type_id: TypeId, type_table: &mut TypeTable) -> TypeId {
        // First check direct substitution
        if let Some(&new_id) = self.structs.type_substitutions.get(&type_id) {
            return new_id;
        }

        // Handle container types that may contain GenericInstance
        match type_table.get(type_id).clone() {
            ResolvedType::BuiltinArray(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.intern(ResolvedType::BuiltinArray(new_inner_id))
                }
            }
            ResolvedType::Ref(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.make_ref(new_inner_id)
                }
            }
            ResolvedType::MutRef(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.make_mut_ref(new_inner_id)
                }
            }
            ResolvedType::Tuple(elem_ids) => {
                let new_elem_ids: Vec<TypeId> = elem_ids
                    .iter()
                    .map(|&id| self.rewrite_type_id(id, type_table))
                    .collect();
                if new_elem_ids == elem_ids {
                    type_id
                } else {
                    type_table.make_tuple(new_elem_ids)
                }
            }
            // Handle GenericInstance types that weren't in the direct substitution map
            // This can happen when function substitution creates new GenericInstance types
            // with different TypeIds for the type arguments
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Skip Array - it has special codegen handling and should remain
                // as GenericInstance, not be rewritten to Struct
                if name == "Array" {
                    return type_id;
                }

                // Build the mangled name using type names (not TypeIds)
                let type_names: Vec<String> = type_args
                    .iter()
                    .map(|&arg| type_table.mangle_type_name(arg))
                    .collect();
                let mangled_name = mangle_generic_name(&name, &type_names);

                // Look for existing Struct with this mangled name via O(1) index
                if let Some(tid) = type_table.find_struct_by_name(&mangled_name) {
                    return tid;
                }
                // If not found, return original type_id
                type_id
            }
            _ => type_id,
        }
    }

    /// Collect all `GenericInstance` types from the type table
    /// Only collects types whose base struct is in `valid_struct_names`
    fn collect_instantiation_sites(
        &mut self,
        type_table: &TypeTable,
        valid_struct_names: &IndexSet<String>,
    ) {
        for id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(id)
            {
                // Skip empty type_args (invalid generic instances)
                if type_args.is_empty() {
                    continue;
                }

                // Skip Array - it has special codegen handling and should not be
                // monomorphized as a regular struct
                if name == "Array" {
                    continue;
                }

                // Only collect if the struct is in our valid set
                // This prevents library modules from trying to instantiate entry module's structs
                if !valid_struct_names.contains(name) {
                    continue;
                }

                // Only process if all type args are concrete (no TypeParams)
                let all_concrete = type_args
                    .iter()
                    .all(|&arg| !type_table.contains_type_param(arg));

                if all_concrete {
                    let key = InstantiationKey {
                        name: name.clone(),
                        impl_type_args: type_args.clone(),
                        method_type_args: vec![],
                        method_info: None, // Struct instantiation,
                    };

                    if !self.structs.instantiated.contains_key(&key) {
                        let mangled = self.instantiation_name(&key, type_table);
                        self.structs
                            .instantiated
                            .insert(key.clone(), mangled.clone());
                        self.structs.mangled_to_key.insert(mangled, key.clone());
                        self.structs.pending.push(key);
                    }
                }
            }
        }
    }

    /// Generate monomorphized struct name: `Box` + `[i32]` -> `"Box<i32>"`
    fn instantiation_name(&self, key: &InstantiationKey, type_table: &TypeTable) -> String {
        let args: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|&t| type_table.mangle_type_name(t))
            .collect();
        mangle_generic_name(&key.name, &args)
    }

    /// Instantiate a generic struct with concrete type arguments
    fn instantiate_struct(
        &mut self,
        generic: &TirStruct,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirStruct> {
        let mangled_name = self.structs.instantiated.get(key)?.clone();

        // Find the GenericInstance's module_source from the type table.
        // Use the generic's original module (where it was defined) for the struct type,
        // ensuring consistency across modules that share the same type table.
        let struct_module_source = type_table
            .iter_type_ids()
            .find_map(|id| {
                if let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } = type_table.get(id)
                    && name == &key.name
                    && type_args == &key.impl_type_args
                {
                    Some(module_source.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| self.current_module_source.clone());

        // Register the concrete struct type in the type table BEFORE substituting field types.
        // This is critical for self-referential structs like:
        //   struct Node<T> { left: Option<&mut Node<T>>, right: Option<&mut Node<T>> }
        // When substituting field types, the inner Node<T> needs to resolve to the
        // monomorphized struct type, not a GenericInstance.
        let concrete_type_id = type_table.make_monomorphized_struct(
            mangled_name.clone(),
            struct_module_source,
            key.name.clone(), // base_name: the original generic struct name
        );

        // Find the GenericInstance TypeId and record the substitution early
        // so that substitute_type can use it for self-references
        for id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(id)
                && name == &key.name
                && type_args == &key.impl_type_args
            {
                self.structs.type_substitutions.insert(id, concrete_type_id);
                self.structs.type_to_mangled_name.insert(
                    id,
                    self.structs
                        .instantiated
                        .get(key)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
        }

        // Build substitution map: type param index -> concrete type
        let substitution: IndexMap<u32, TypeId> = generic
            .type_params
            .iter()
            .zip(key.impl_type_args.iter())
            .map(|(param, &arg)| (param.index, arg))
            .collect();

        // Substitute types in fields (now self-references can be resolved)
        let fields: Vec<TirField> = generic
            .fields
            .iter()
            .map(|field| {
                let new_type_id = self.substitute_type(field.type_id, &substitution, type_table);
                TirField {
                    name: field.name.clone(),
                    is_pub: field.is_pub,
                    type_id: new_type_id,
                    index: field.index,
                    span: field.span,
                    is_hidden: field.is_hidden,
                    serde_rename: field.serde_rename.clone(),
                    serde_default: field.serde_default,
                }
            })
            .collect();

        // Create the monomorphized struct
        let concrete = TirStruct {
            name: mangled_name,
            is_pub: generic.is_pub,
            type_params: vec![], // Concrete struct has no type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                impl_type_args: key.impl_type_args.clone(),
                method_type_args: key.method_type_args.clone(),
                is_blanket: false,
            }),
            fields,
            span: generic.span,
            serde_rename_all: generic.serde_rename_all.clone(),
        };

        Some(concrete)
    }

    /// Substitute type parameters in a type with concrete types
    fn substitute_type(
        &self,
        type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::TypeParam { index, name } => {
                // Direct substitution
                *substitution.get(&index).unwrap_or_else(|| {
                    panic!("TypeParam `{name}` (index {index}) not found in substitution map")
                })
            }
            ResolvedType::TypePack { index, name } => {
                // Direct substitution (the substituted type is typically a tuple)
                *substitution.get(&index).unwrap_or_else(|| {
                    panic!("TypePack `..{name}` (index {index}) not found in substitution map")
                })
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type(elem, substitution, type_table);
                type_table.intern(ResolvedType::BuiltinArray(new_elem))
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
                let mut new_elems: Vec<TypeId> = Vec::new();
                for &e in &elems {
                    match type_table.get(e).clone() {
                        ResolvedType::TypePack { index, name } => {
                            // Splice: the substituted type for a pack is a tuple; expand its elements
                            let &pack_type = substitution.get(&index).unwrap_or_else(|| {
                                panic!(
                                    "TypePack `..{name}` (index {index}) not found in substitution map"
                                )
                            });
                            match type_table.get(pack_type).clone() {
                                ResolvedType::Tuple(pack_elems) => {
                                    new_elems.extend_from_slice(&pack_elems);
                                }
                                _ => {
                                    // Single type (not a tuple) — treat as one-element pack
                                    new_elems.push(pack_type);
                                }
                            }
                        }
                        _ => {
                            new_elems.push(self.substitute_type(e, substitution, type_table));
                        }
                    }
                }
                type_table.make_tuple(new_elems)
            }
            ResolvedType::Function {
                params,
                return_type,
                effects,
                stores,
            } => {
                // Substitute type parameters in function parameter types and return type
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute_type(p, substitution, type_table))
                    .collect();
                let new_return_type = self.substitute_type(return_type, substitution, type_table);
                type_table.make_function(new_params, new_return_type, effects, stores)
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // Handle invalid GenericInstance with empty type_args
                // This can occur when a generic type is referenced without type arguments in its own methods
                // e.g., in Container<T>::new(), the return value Container { ... } has type Container<T>
                // but may be represented as GenericInstance with empty type_args
                if type_args.is_empty() {
                    // If we're in a substitution context (substitution map is not empty),
                    // try to infer the type args from the substitution
                    if !substitution.is_empty() {
                        // Build mangled name using ALL values in substitution map
                        // Sort by param index to get correct order
                        let mut indexed_args: Vec<(u32, TypeId)> =
                            substitution.iter().map(|(&idx, &tid)| (idx, tid)).collect();
                        indexed_args.sort_by_key(|(idx, _)| *idx);

                        // Build name using new format: Name<Type1,Type2>
                        let type_names: Vec<String> = indexed_args
                            .iter()
                            .map(|(_, arg_id)| type_table.mangle_type_name(*arg_id))
                            .collect();
                        let mangled_name = mangle_generic_name(&name, &type_names);

                        // Look for monomorphized struct with this name via O(1) index
                        if let Some(tid) = type_table.find_struct_by_name(&mangled_name) {
                            return tid;
                        }
                    }

                    // Fallback: look for plain struct by name
                    if let Some(tid) = type_table.find_struct_by_name(&name) {
                        return tid;
                    }
                    // If not found, just return the original type_id
                    return type_id;
                }

                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute_type(arg, substitution, type_table))
                    .collect();

                // Check if there's already a monomorphized struct for this instance
                // Build the mangled name: Container<i32> (using type names)
                let type_names: Vec<String> = new_args
                    .iter()
                    .map(|&arg| type_table.mangle_type_name(arg))
                    .collect();
                let mangled_name = mangle_generic_name(&name, &type_names);

                // Look for existing struct with this name via O(1) index
                if let Some(tid) = type_table.find_struct_by_name(&mangled_name) {
                    return tid;
                }

                // Fallback to GenericInstance if no monomorphized struct found
                type_table.make_generic_instance(name, module_source, new_args)
            }
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => {
                // Substitute the underlying type param to get the concrete type
                let concrete_id = self.substitute_type(param_id, substitution, type_table);
                if concrete_id != param_id {
                    // Direct lookup for pre-registered concrete types
                    if let Some(resolved) = type_table.resolve_assoc_type(concrete_id, &assoc_name)
                    {
                        return resolved;
                    }
                    // Fallback for GenericInstance types: resolve using generic definitions
                    if let Some(resolved) =
                        type_table.resolve_generic_assoc_type(concrete_id, &assoc_name)
                    {
                        return resolved;
                    }
                }
                // Fallback: return the original type (projection unresolved)
                type_id
            }
            // Other types don't contain type parameters
            _ => type_id,
        }
    }

    /// Collect function instantiation sites from Call/MethodCall/StaticCall expressions
    fn collect_function_instantiation_sites(
        &mut self,
        module: &TirModule,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    ) {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Skip generic functions - their bodies contain TypeParam references that
            // would incorrectly queue instantiations with TypeParam TypeIds instead of
            // concrete types. We only scan concrete functions; generic function bodies
            // are scanned after instantiation in Phase 9.
            // Effect-only params don't count as generic.
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                continue;
            }
            if let Some(body) = &func.body {
                self.collect_func_instantiation_sites_in_block(
                    body,
                    generic_functions,
                    &module.type_table.borrow(),
                );
            }
        }

        // Also scan global variable initializers for function instantiation sites
        for global in &module.globals {
            self.collect_func_instantiation_sites_in_expr(
                &global.initializer,
                generic_functions,
                &module.type_table.borrow(),
            );
        }
    }

    fn collect_func_instantiation_sites_in_block(
        &mut self,
        block: &TirBlock,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
    ) {
        for stmt in &block.stmts {
            self.collect_func_instantiation_sites_in_stmt(stmt, generic_functions, type_table);
        }
    }

    fn collect_func_instantiation_sites_in_stmt(
        &mut self,
        stmt: &TirStmt,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
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
            TirStmtKind::Loop { body } => {
                self.collect_func_instantiation_sites_in_block(body, generic_functions, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.collect_func_instantiation_sites_in_expr(v, generic_functions, type_table);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.collect_func_instantiation_sites_in_block(
                    block,
                    generic_functions,
                    type_table,
                );
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    scrutinee,
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
            TirStmtKind::LetDestructure { value, .. } => {
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.collect_func_instantiation_sites_in_expr(
                    iterable,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(body, generic_functions, type_table);
            }
        }
    }

    fn collect_func_instantiation_sites_in_expr(
        &mut self,
        expr: &TirExpr,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
    ) {
        match &expr.kind {
            TirExprKind::Call {
                func,
                type_args,
                args,
                ..
            } => {
                let qualified_func_name =
                    generic_function_key(func.is_method(), &func.module_source, &func.name);
                // Check if this is a call to a generic function with explicit type args
                if !type_args.is_empty() && generic_functions.contains_key(&qualified_func_name) {
                    let key = InstantiationKey {
                        name: qualified_func_name,
                        impl_type_args: vec![],
                        method_type_args: type_args.clone(),
                        method_info: func.method_info.clone(),
                    };
                    if !self.functions.instantiated.contains_key(&key) {
                        let mangled = self.function_instantiation_name(&key, type_table);
                        self.functions
                            .instantiated
                            .insert(key.clone(), mangled.clone());
                        self.functions.mangled_to_key.insert(mangled, key.clone());
                        self.functions.pending.push(key);
                    }
                }
                // Also check if this is a static method call on a monomorphized struct
                // (formerly StaticCall). Use method_info metadata to get struct/method name.
                if let FunctionRef {
                    method_info: Some(info),
                    monomorph_info: Some(monomorph),
                    ..
                } = func
                    && (!monomorph.impl_type_args.is_empty()
                        || !monomorph.method_type_args.is_empty())
                {
                    let mut names_to_try = vec![MethodName::format_local(
                        &info.base_struct_name,
                        info.trait_name.as_deref(),
                        &info.method_name,
                    )];
                    if info.struct_name != info.base_struct_name {
                        names_to_try.push(MethodName::format_local(
                            &info.struct_name,
                            info.trait_name.as_deref(),
                            &info.method_name,
                        ));
                    }
                    for generic_method_name in names_to_try {
                        if let Some(generic_func_rc) = generic_functions.get(&generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            // impl_type_args and method_type_args are now separate.
                            // For variadic impls, impl_type_args may be empty — extract
                            // from the struct name if needed.
                            let impl_type_args = if monomorph.impl_type_args.is_empty()
                                && !generic_func.impl_type_params.is_empty()
                                && info.struct_name != info.base_struct_name
                            {
                                type_table
                                    .find_type_args_by_mangled_name(&info.struct_name)
                                    .unwrap_or_default()
                            } else {
                                monomorph.impl_type_args.clone()
                            };
                            let method_type_args = monomorph.method_type_args.clone();
                            let total = impl_type_args.len() + method_type_args.len();
                            if total >= generic_func.impl_type_params.len() {
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name,
                                    impl_type_args: impl_type_args.clone(),
                                    method_type_args,
                                    method_info,
                                };
                                if !self.functions.instantiated.contains_key(&key) {
                                    let mangled = self.method_instantiation_name(
                                        &key,
                                        type_table,
                                        impl_type_args.len(),
                                    );
                                    self.functions
                                        .instantiated
                                        .insert(key.clone(), mangled.clone());
                                    self.functions.mangled_to_key.insert(mangled, key.clone());
                                    self.functions.pending.push(key);
                                }
                            }
                            break;
                        }
                    }
                }
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        &arg.expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                type_args,
                args,
                ..
            } => {
                // Extract method name from method_info or fall back to function name
                let method_name = method_func
                    .method_info
                    .clone()
                    .map(|info| info.method_name)
                    .unwrap_or_else(|| method_func.name.clone());
                // Check if this is a method call with explicit type args
                if !type_args.is_empty() {
                    // Get the struct name from the receiver type
                    if let Some(struct_name) =
                        self.get_struct_name_from_type(receiver.type_id, type_table)
                    {
                        // Try both inherent method and trait method formats
                        let trait_name_opt = method_func
                            .method_info
                            .clone()
                            .and_then(|info| info.trait_name);
                        let mut names_to_try: Vec<(String, Option<String>)> = vec![(
                            MethodName::format_local(&struct_name, None, &method_name),
                            None,
                        )];
                        if let Some(ref tn) = trait_name_opt {
                            names_to_try.push((
                                MethodName::format_local(&struct_name, Some(tn), &method_name),
                                Some(tn.clone()),
                            ));
                        }

                        let mut found = false;
                        for (full_method_name, tn) in &names_to_try {
                            if let Some(gf) = generic_functions.get(full_method_name) {
                                let method_info =
                                    gf.borrow().method_info.clone().unwrap_or_else(|| {
                                        LocalMethodName::new(
                                            struct_name.clone(),
                                            tn.clone(),
                                            method_name.clone(),
                                        )
                                    });
                                let key = InstantiationKey {
                                    name: full_method_name.clone(),
                                    impl_type_args: vec![],
                                    method_type_args: type_args.clone(),
                                    method_info: Some(method_info),
                                };
                                if !self.functions.instantiated.contains_key(&key) {
                                    let mangled = self.method_instantiation_name(
                                        &key, type_table,
                                        0, // non-generic struct: no impl type params
                                    );
                                    self.functions
                                        .instantiated
                                        .insert(key.clone(), mangled.clone());
                                    self.functions.mangled_to_key.insert(mangled, key.clone());
                                    self.functions.pending.push(key);
                                }
                                found = true;
                                break;
                            }
                        }
                        // Handle "double generics": method call with type_args on a monomorphized generic struct
                        // e.g., c.transform::<i64>(100) where c: Container<i32> and transform<U>
                        // Also handles GenericInstance receivers (e.g., Option<i32>)
                        if !found {
                            let base_info = self
                                .structs
                                .mangled_to_key
                                .get(&struct_name)
                                .map(|k| (k.name.clone(), k.impl_type_args.clone()))
                                .or_else(|| {
                                    self.get_struct_info_from_type(receiver.type_id, type_table)
                                        .filter(|(_, args)| !args.is_empty())
                                });
                            if let Some((base_struct, impl_type_args)) = base_info {
                                // Try both inherent and trait method formats
                                let mut dg_names: Vec<(String, Option<String>)> = vec![(
                                    MethodName::format_local(&base_struct, None, &method_name),
                                    None,
                                )];
                                if let Some(ref tn) = trait_name_opt {
                                    dg_names.push((
                                        MethodName::format_local(
                                            &base_struct,
                                            Some(tn),
                                            &method_name,
                                        ),
                                        Some(tn.clone()),
                                    ));
                                    // For ref-type impls, also try "&^Trait::method"
                                    if let Some(ref info) = method_func.method_info
                                        && info.base_struct_name != base_struct
                                    {
                                        dg_names.push((
                                            MethodName::format_local(
                                                &info.base_struct_name,
                                                Some(tn),
                                                &method_name,
                                            ),
                                            Some(tn.clone()),
                                        ));
                                    }
                                }

                                for (generic_method_name, tn) in &dg_names {
                                    if let Some(generic_func_rc) =
                                        generic_functions.get(generic_method_name)
                                    {
                                        let generic_func = generic_func_rc.borrow();
                                        if impl_type_args.len()
                                            >= generic_func.impl_type_params.len()
                                        {
                                            let method_info = LocalMethodName::new(
                                                base_struct,
                                                tn.clone(),
                                                method_name.clone(),
                                            );
                                            let key = InstantiationKey {
                                                name: generic_method_name.clone(),
                                                impl_type_args: impl_type_args.clone(),
                                                method_type_args: type_args.clone(),
                                                method_info: Some(method_info),
                                            };
                                            if !self.functions.instantiated.contains_key(&key) {
                                                let mangled = self.method_instantiation_name(
                                                    &key,
                                                    type_table,
                                                    impl_type_args.len(),
                                                );
                                                self.functions
                                                    .instantiated
                                                    .insert(key.clone(), mangled.clone());
                                                self.functions
                                                    .mangled_to_key
                                                    .insert(mangled, key.clone());
                                                self.functions.pending.push(key);
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Also check if the receiver is a monomorphized generic struct
                // e.g., c.get() where c: Counter<i32>, or arr.append() where arr: Array<fn(i32)->i32>
                let struct_info = self.get_struct_info_from_type(receiver.type_id, type_table);
                if let Some((base_struct, impl_type_args)) = struct_info
                    && !impl_type_args.is_empty()
                {
                    // Try both regular method and trait method formats
                    // Method names to try: BaseStruct::method, BaseStruct^Trait::method (from method_info)
                    let mut names_to_try = Vec::new();
                    // For ref-type impls (e.g., impl Trait for &Array<T>),
                    // try the ref struct name FIRST so it takes priority
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                        && info.base_struct_name != base_struct
                    {
                        names_to_try.push(MethodName::format_local(
                            &info.base_struct_name,
                            Some(trait_name),
                            &method_name,
                        ));
                    }
                    names_to_try.push(MethodName::format_local(&base_struct, None, &method_name));
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                    {
                        names_to_try.push(MethodName::format_local(
                            &base_struct,
                            Some(trait_name),
                            &method_name,
                        ));
                    }

                    for generic_method_name in &names_to_try {
                        if let Some(generic_func_rc) = generic_functions.get(generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            // Check if method has its own type params (double generics)
                            let has_method_type_params = generic_func.has_real_type_params();
                            // Queue if we have at least enough impl type args.
                            // impl_type_args may be longer than impl_type_params when the impl
                            // fixes some struct type params to concrete types
                            // (e.g., `impl Trait for Foo<Array<String>, V>` where only V is free).
                            if impl_type_args.len() >= generic_func.impl_type_params.len() {
                                let method_type_args_for_key =
                                    if has_method_type_params && !type_args.is_empty() {
                                        type_args.clone()
                                    } else {
                                        vec![]
                                    };
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name.clone(),
                                    impl_type_args: impl_type_args.clone(),
                                    method_type_args: method_type_args_for_key,
                                    method_info,
                                };
                                if !self.functions.instantiated.contains_key(&key) {
                                    let mangled = self.method_instantiation_name(
                                        &key,
                                        type_table,
                                        impl_type_args.len(),
                                    );
                                    self.functions
                                        .instantiated
                                        .insert(key.clone(), mangled.clone());
                                    self.functions.mangled_to_key.insert(mangled, key.clone());
                                    self.functions.pending.push(key);
                                }
                                break;
                            }
                        }
                    }
                }

                // Also handle already-monomorphized structs via reverse lookup
                // e.g., c.add(10) where c: Container<i32>
                // Use get_struct_name_from_type to properly unwrap reference types (&T, &mut T)
                if let Some(struct_name) =
                    self.get_struct_name_from_type(receiver.type_id, type_table)
                    && let Some(struct_key) = self.structs.mangled_to_key.get(&struct_name)
                {
                    let base_struct = &struct_key.name;
                    let impl_type_args = struct_key.impl_type_args.clone();

                    let mut names_to_try =
                        vec![MethodName::format_local(base_struct, None, &method_name)];
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                    {
                        names_to_try.push(MethodName::format_local(
                            base_struct,
                            Some(trait_name),
                            &method_name,
                        ));
                        // For ref-type impls (e.g., impl Trait for &Array<T>),
                        // the template function is registered under "&^Trait::method"
                        if info.base_struct_name != *base_struct {
                            names_to_try.push(MethodName::format_local(
                                &info.base_struct_name,
                                Some(trait_name),
                                &method_name,
                            ));
                        }
                    }

                    for generic_method_name in names_to_try {
                        if let Some(generic_func_rc) = generic_functions.get(&generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            let has_method_type_params = generic_func.has_real_type_params();
                            if impl_type_args.len() >= generic_func.impl_type_params.len() {
                                let method_type_args_for_key =
                                    if has_method_type_params && !type_args.is_empty() {
                                        type_args.clone()
                                    } else {
                                        vec![]
                                    };
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name.clone(),
                                    impl_type_args: impl_type_args.clone(),
                                    method_type_args: method_type_args_for_key,
                                    method_info,
                                };
                                if !self.functions.instantiated.contains_key(&key) {
                                    let mangled = self.method_instantiation_name(
                                        &key,
                                        type_table,
                                        impl_type_args.len(),
                                    );
                                    self.functions
                                        .instantiated
                                        .insert(key.clone(), mangled.clone());
                                    self.functions.mangled_to_key.insert(mangled, key.clone());
                                    self.functions.pending.push(key);
                                }
                                break;
                            }
                        }
                    }
                }

                // Blanket impl fallback: if the FunctionRef has monomorph_info from a
                // blanket impl that matches a generic function template, queue the
                // instantiation using that template function.
                if let FunctionRef {
                    monomorph_info: Some(mono),
                    ..
                } = method_func
                    && mono.is_blanket
                    && let Some(generic_func_rc) = generic_functions.get(&mono.generic_name)
                {
                    let generic_func = generic_func_rc.borrow();
                    let method_info = generic_func.method_info.clone();
                    // impl_type_args and method_type_args are now separate in MonomorphInfo.
                    // For blanket impls, impl_type_args contains the concrete receiver type.
                    // method_type_args comes from the MethodCall's type_args field.
                    // If method_type_args is empty but the callee has type params, infer from args.
                    let impl_ta = mono.impl_type_args.clone();
                    let method_ta = if !type_args.is_empty() {
                        type_args.clone()
                    } else if mono.method_type_args.is_empty()
                        && generic_func.has_real_type_params()
                    {
                        // Infer method type args from argument types
                        let method_params = &generic_func.params;
                        let mut inferred_method_args = Vec::new();
                        for param in &generic_func.type_params {
                            let param_idx =
                                generic_func.impl_type_params.len() as u32 + param.index;
                            let mut inferred = None;
                            for (pi, mp) in method_params.iter().enumerate().skip(1) {
                                let inner = match type_table.get(mp.type_id) {
                                    ResolvedType::Ref(t) | ResolvedType::MutRef(t) => *t,
                                    _ => mp.type_id,
                                };
                                if matches!(type_table.get(inner), ResolvedType::TypeParam { index, .. } if *index == param_idx)
                                {
                                    let arg_idx = pi - 1;
                                    if let Some(arg) = args.get(arg_idx) {
                                        let mut arg_type = arg.expr.type_id;
                                        while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                            type_table.get(arg_type).clone()
                                        {
                                            arg_type = t;
                                        }
                                        if let ResolvedType::GenericInstance {
                                            name,
                                            type_args: ta,
                                            ..
                                        } = type_table.get(arg_type)
                                            && name == "Box"
                                            && ta.len() == 1
                                        {
                                            arg_type = ta[0];
                                        }
                                        inferred = Some(arg_type);
                                    }
                                    break;
                                }
                            }
                            if let Some(tid) = inferred {
                                inferred_method_args.push(tid);
                            }
                        }
                        inferred_method_args
                    } else {
                        mono.method_type_args.clone()
                    };
                    let key = InstantiationKey {
                        name: mono.generic_name.clone(),
                        impl_type_args: impl_ta,
                        method_type_args: method_ta,
                        method_info,
                    };
                    if !self.functions.instantiated.contains_key(&key) {
                        let impl_type_params_count = generic_func.impl_type_params.len();
                        let mangled = self.method_instantiation_name_inner(
                            &key,
                            type_table,
                            impl_type_params_count,
                            &generic_func.impl_type_params,
                        );
                        self.functions
                            .instantiated
                            .insert(key.clone(), mangled.clone());
                        self.functions.mangled_to_key.insert(mangled, key.clone());
                        self.functions.pending.push(key);
                    }
                }

                self.collect_func_instantiation_sites_in_expr(
                    receiver,
                    generic_functions,
                    type_table,
                );
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        &arg.expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
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
            TirExprKind::TupleLiteral { elements } => {
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
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
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
                    if let Some(guard) = &arm.guard {
                        self.collect_func_instantiation_sites_in_expr(
                            guard,
                            generic_functions,
                            type_table,
                        );
                    }
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
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_func_instantiation_sites_in_expr(
                    functor,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_func_instantiation_sites_in_expr(
                        payload_expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_func_instantiation_sites_in_block(
                    block,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirExprKind::VariantTag { expr } => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirExprKind::VariantPayload { expr, .. } => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    scrutinee,
                    generic_functions,
                    type_table,
                );
                for arm in arms {
                    self.collect_func_instantiation_sites_in_block(
                        arm,
                        generic_functions,
                        type_table,
                    );
                }
                self.collect_func_instantiation_sites_in_block(
                    default,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.collect_func_instantiation_sites_in_expr(
                            inner,
                            generic_functions,
                            type_table,
                        );
                    }
                }
            }
            // Literals and simple expressions
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
        }
    }

    /// Get the struct name from a `type_id`, unwrapping references if needed
    /// For generic instances, returns the mangled name with type args (e.g., "Array<i32>")
    fn get_struct_name_from_type(&self, type_id: TypeId, type_table: &TypeTable) -> Option<String> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. } => Some(name.clone()),
            ResolvedType::Primitive(prim) => Some(prim.as_str().to_string()),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Return the mangled name with type args (e.g., "Array<i32>", "Box<String>")
                let args: Vec<String> = type_args
                    .iter()
                    .map(|arg| type_table.mangle_type_name(*arg))
                    .collect();
                Some(mangle_generic_name(name, &args))
            }
            ResolvedType::Tuple(elems) => {
                let args: Vec<String> = elems
                    .iter()
                    .map(|t| type_table.mangle_type_name(*t))
                    .collect();
                Some(mangle_generic_name("Tuple", &args))
            }
            ResolvedType::BuiltinArray(elem) => {
                let arg = type_table.mangle_type_name(*elem);
                Some(mangle_generic_name("Array", &[arg]))
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_name_from_type(*inner, type_table)
            }
            _ => None,
        }
    }

    /// Get the base struct name and type args from a `type_id`, unwrapping references if needed
    /// Returns (`base_name`, `type_args`) for `GenericInstance`, (name, []) for Struct
    fn get_struct_info_from_type(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<(String, Vec<TypeId>)> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. } => {
                // For monomorphized structs with names like "Array<i32>", look up the
                // original InstantiationKey to get the base name and type_args
                if let Some(key) = self.structs.mangled_to_key.get(name) {
                    Some((key.name.clone(), key.impl_type_args.clone()))
                } else {
                    Some((name.clone(), vec![]))
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => Some((name.clone(), type_args.clone())),
            ResolvedType::Tuple(elems) => Some(("Tuple".to_string(), elems.clone())),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_info_from_type(*inner, type_table)
            }
            _ => None,
        }
    }

    /// Generate instantiated function name: `identity` + `[i32]` -> `"identity<i32>"`
    fn function_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
    ) -> String {
        // For free functions, all type args are method-level.
        // For fallback from method_instantiation_name_inner (no method_info),
        // combine both for backwards-compatible naming.
        let mut args: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();
        args.extend(
            key.method_type_args
                .iter()
                .map(|t| type_table.mangle_type_name(*t)),
        );
        mangle_generic_name(&key.name, &args)
    }

    /// Generate instantiated method name
    /// Format: `StructWithImplArgs::methodWithMethodArgs`
    /// e.g., `Container::transform` with `[i32, i64]` and `impl_type_params_count=1` -> `"Container<i32>::transform<i64>"`
    fn method_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params_count: usize,
    ) -> String {
        self.method_instantiation_name_inner(key, type_table, impl_type_params_count, &[])
    }

    fn method_instantiation_name_inner(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params_count: usize,
        impl_type_params: &[crate::tir::TirTypeParam],
    ) -> String {
        // Use method_info metadata instead of parsing key.name
        let Some(ref method_info) = key.method_info else {
            // Fallback to regular function naming if no method_info
            return self.function_instantiation_name(key, type_table);
        };

        // impl_type_args and method_type_args are now separate in InstantiationKey
        let _ = impl_type_params_count; // no longer needed for split

        let impl_arg_names: Vec<String> = key
            .impl_type_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();

        // Blanket impl: struct name IS the type param (e.g., "I").
        // Detected by checking if base_struct_name matches an impl type param name.
        let is_blanket = impl_type_params
            .iter()
            .any(|p| p.name == method_info.base_struct_name);

        let mangled_struct = if is_blanket && !impl_arg_names.is_empty() {
            // Replace struct name entirely: "I" → "StrCharIter"
            MethodName::format_struct_with_args(
                &impl_arg_names[0],
                &[],
                method_info.trait_name.as_deref(),
            )
        } else {
            // Normal: append type args: "Array" → "Array<i32>"
            MethodName::format_struct_with_args(
                &method_info.struct_name,
                &impl_arg_names,
                method_info.trait_name.as_deref(),
            )
        };

        // Build method name: transform<i64> (using method type args)
        let method_arg_names: Vec<String> = key
            .method_type_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();
        let mangled_method =
            MethodName::format_method_with_args(&method_info.method_name, &method_arg_names);

        MethodName::join_struct_method(&mangled_struct, &mangled_method)
    }

    /// Instantiate a generic function with concrete type arguments
    fn instantiate_function(
        &mut self,
        generic: &TirFunction,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirFunction> {
        let mangled_name = self.functions.instantiated.get(key)?.clone();

        // Build substitution map: type param index -> concrete type
        // Include both method-level type params AND impl block type params
        let mut substitution: IndexMap<u32, TypeId> = IndexMap::default();

        // Add impl block type params from key.impl_type_args
        let non_pack_impl_params_count = generic
            .impl_type_params
            .iter()
            .filter(|p| !p.is_pack)
            .count();
        for param in &generic.impl_type_params {
            if param.is_pack {
                // Variadic pack: map the pack index to a tuple of the impl-level type args,
                // excluding non-pack impl params.
                let pack_args_count = key
                    .impl_type_args
                    .len()
                    .saturating_sub(non_pack_impl_params_count);
                let pack_args: Vec<TypeId> = key
                    .impl_type_args
                    .iter()
                    .take(pack_args_count)
                    .copied()
                    .collect();
                let pack_type = type_table.make_tuple(pack_args);
                substitution.insert(param.index, pack_type);
            } else if let Some(&arg) = key.impl_type_args.get(param.index as usize) {
                substitution.insert(param.index, arg);
            }
        }

        // Add method-level type params from key.method_type_args
        let offset = generic.impl_type_params.len() as u32;
        for (param, &arg) in generic.type_params.iter().zip(key.method_type_args.iter()) {
            substitution.insert(offset + param.index, arg);
        }

        // Substitute types in parameters
        let params: Vec<TirParam> = generic
            .params
            .iter()
            .map(|param| TirParam {
                name: param.name.clone(),
                type_id: self.substitute_type(param.type_id, &substitution, type_table),
                local_index: param.local_index,
                is_mut: param.is_mut,
                span: param.span,
            })
            .collect();

        // Substitute return type
        let return_type = self.substitute_type(generic.return_type, &substitution, type_table);

        // Substitute types in local_types
        let mut local_types: Vec<TypeId> = generic
            .local_types
            .iter()
            .map(|&t| self.substitute_type(t, &substitution, type_table))
            .collect();

        // Clone and substitute types in body
        let mut local_count = generic.local_count;
        let body = generic.body.as_ref().map(|b| {
            let mut new_body = b.clone();
            self.substitute_types_in_block(&mut new_body, &substitution, type_table);
            // Fixup VariadicForOf: allocate separate locals for each element iteration
            Self::fixup_variadic_for_of_locals(&mut new_body, &mut local_count, &mut local_types);
            // Fixup TypePackExpansion: allocate separate locals for each expanded element
            Self::fixup_pack_expansion_locals(&mut new_body, &mut local_count, &mut local_types);
            new_body
        });

        Some(TirFunction {
            is_async: generic.is_async,
            name: mangled_name,
            is_pub: generic.is_pub,
            is_export: generic.is_export, // Inherit from generic
            type_params: vec![],          // Concrete function has no type params
            impl_type_params: vec![],     // Already monomorphized, no impl type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                impl_type_args: key.impl_type_args.clone(),
                method_type_args: key.method_type_args.clone(),
                is_blanket: false,
            }),
            // Update method_info with mangled struct name including impl type args
            // and method type args (from the method's own type params)
            method_info: generic.method_info.as_ref().map(|info| {
                let impl_type_arg_names: Vec<String> = key
                    .impl_type_args
                    .iter()
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                let method_type_arg_names: Vec<String> = key
                    .method_type_args
                    .iter()
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                // Blanket impl: struct name IS the type param (e.g., "I").
                // Replace it with the concrete type name instead of appending type args.
                let is_blanket = generic
                    .impl_type_params
                    .iter()
                    .any(|p| p.name == info.base_struct_name);
                if is_blanket && !impl_type_arg_names.is_empty() {
                    let base = type_table.base_type_name(key.impl_type_args[0]);
                    info.with_substituted_struct_name(&impl_type_arg_names[0], &base)
                } else {
                    info.with_type_args(&impl_type_arg_names, &method_type_arg_names)
                }
            }),
            params,
            return_type,
            effects: generic.effects.clone(),
            stores: generic.stores.clone(),
            body,
            span: generic.span,
            local_count,
            local_types,
            address_taken_locals: generic.address_taken_locals.clone(),
            stores_aliased_locals: generic.stores_aliased_locals.clone(),
            // Scratch local fields - computed by lower phase (after monomorphization)
            is_cm_binding: false,
            inline_hint: generic.inline_hint,
            comp_features: generic.comp_features,
            export_name: generic.export_name.clone(),
            allocator_tag: generic.allocator_tag.clone(),
        })
    }

    /// Substitute type parameters in a block
    fn substitute_types_in_block(
        &self,
        block: &mut TirBlock,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        let has_variadic = block
            .stmts
            .iter()
            .any(|s| matches!(&s.kind, TirStmtKind::VariadicForOf { .. }));
        if has_variadic {
            let old_stmts = std::mem::take(&mut block.stmts);
            for mut stmt in old_stmts {
                if let TirStmtKind::VariadicForOf { .. } = &stmt.kind {
                    block.stmts.extend(self.expand_variadic_for_of(
                        &mut stmt,
                        substitution,
                        type_table,
                    ));
                } else {
                    self.substitute_types_in_stmt(&mut stmt, substitution, type_table);
                    block.stmts.push(stmt);
                }
            }
        } else {
            for stmt in &mut block.stmts {
                self.substitute_types_in_stmt(stmt, substitution, type_table);
            }
        }
    }

    fn substitute_types_in_stmt(
        &self,
        stmt: &mut TirStmt,
        substitution: &IndexMap<u32, TypeId>,
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
            TirStmtKind::Loop { body } => {
                self.substitute_types_in_block(body, substitution, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.substitute_types_in_expr(v, substitution, type_table);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.substitute_types_in_block(block, substitution, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.substitute_types_in_expr(scrutinee, substitution, type_table);
                self.substitute_types_in_pattern(pattern, substitution, type_table);
                self.substitute_types_in_block(then_block, substitution, type_table);
                if let Some(else_blk) = else_block {
                    self.substitute_types_in_block(else_blk, substitution, type_table);
                }
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.substitute_types_in_pattern(pattern, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded in substitute_types_in_block")
            }
        }
    }

    /// Expand a `VariadicForOf` TIR node into concrete unrolled blocks.
    ///
    /// After type substitution resolves `TypePack` to a concrete tuple, this generates
    /// the same structure as the resolver's `resolve_tuple_for_of`.
    fn expand_variadic_for_of(
        &self,
        stmt: &mut TirStmt,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> Vec<TirStmt> {
        let span = stmt.span;
        let TirStmtKind::VariadicForOf {
            iterable,
            binding_name,
            binding_local,
            is_mut,
            body,
            unique_id,
        } = &mut stmt.kind
        else {
            unreachable!()
        };

        // Substitute types in the iterable to get the concrete tuple type
        self.substitute_types_in_expr(iterable, substitution, type_table);

        // Get the concrete tuple elements
        let iterable_type = iterable.type_id;
        let elements = match type_table.get(iterable_type) {
            ResolvedType::Tuple(elems) => elems.clone(),
            other => {
                panic!("VariadicForOf: expected concrete Tuple after substitution, got {other:?}");
            }
        };

        // Find the TypePack index in the substitution map so we can override it per element
        let pack_index = {
            let mut found = None;
            for (&idx, &tid) in substitution {
                if tid == iterable_type || matches!(type_table.get(tid), ResolvedType::Tuple(_)) {
                    found = Some(idx);
                    break;
                }
            }
            found
        };

        let uid = *unique_id;
        let temp_name = format!("__tuple_{uid}");
        let temp_local = *binding_local;

        let mut outer_stmts = Vec::new();

        // let __tuple_N = iterable;
        outer_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: temp_name.clone(),
                local_index: temp_local,
                is_mut: false,
                is_reactive: false,
                type_id: iterable_type,
                value: iterable.clone(),
                skip_value_copy: false,
            },
            span,
        ));

        let binding_local_idx = *binding_local;
        let b_name = binding_name.clone();
        let b_mut = *is_mut;

        // For each element, create: { let v = __tuple_N.i; body }
        for (i, &elem_type) in elements.iter().enumerate() {
            let mut iter_stmts = Vec::new();

            let tuple_ref = TirExpr::new(
                TirExprKind::Local {
                    index: temp_local,
                    name: temp_name.clone(),
                },
                iterable_type,
                span,
            );
            let field = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(tuple_ref),
                    field_index: i as u32,
                    field_name: i.to_string(),
                },
                elem_type,
                span,
            );

            iter_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: b_name.clone(),
                    local_index: binding_local_idx,
                    is_mut: b_mut,
                    is_reactive: false,
                    type_id: elem_type,
                    value: field,
                    skip_value_copy: false,
                },
                span,
            ));

            // Clone the body and substitute types with per-element substitution.
            // Override the TypePack → elem_type (instead of TypePack → tuple type)
            let mut elem_body = body.clone();
            if let Some(pack_idx) = pack_index {
                let mut elem_substitution = substitution.clone();
                elem_substitution.insert(pack_idx, elem_type);
                self.substitute_types_in_block(&mut elem_body, &elem_substitution, type_table);
            } else {
                // Fallback: substitute with original and rewrite manually
                self.substitute_types_in_block(&mut elem_body, substitution, type_table);
                self.rewrite_variadic_binding_types(
                    &mut elem_body,
                    binding_local_idx,
                    elem_type,
                    type_table,
                );
            }
            // Populate type_args on MethodCall nodes that have inferred generic params.
            // Inside variadic for-of, method calls like `seq.element(&v)` have empty type_args
            // because T was inferred from a TypePack at resolution time. Now that types are
            // concrete, fill in type_args so the monomorphizer can instantiate the generic method.
            Self::infer_method_call_type_args(&mut elem_body, type_table);
            iter_stmts.extend(elem_body.stmts);

            let label = format!("__tuple_iter_{uid}_{i}");
            outer_stmts.push(TirStmt::new(
                TirStmtKind::LabeledBlock {
                    label,
                    block: TirBlock::new(iter_stmts, span),
                },
                span,
            ));
        }

        let outer_label = format!("__tuple_for_of_{uid}");
        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: outer_label,
                block: TirBlock::new(outer_stmts, span),
            },
            span,
        )]
    }

    /// After variadic for-of expansion, method calls that had inferred type params
    /// (empty `type_args`) need their `type_args` populated from the concrete argument types.
    /// e.g., `seq.element(&v)` where element<T: Serialize> — infer T from the arg type.
    fn infer_method_call_type_args(block: &mut TirBlock, type_table: &TypeTable) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                TirStmtKind::Expr(expr) => {
                    Self::infer_method_call_type_args_in_expr(expr, type_table);
                }
                TirStmtKind::Let { value, .. } => {
                    Self::infer_method_call_type_args_in_expr(value, type_table);
                }
                TirStmtKind::LabeledBlock { block, .. } => {
                    Self::infer_method_call_type_args(block, type_table);
                }
                TirStmtKind::If {
                    condition,
                    then_block,
                    else_block,
                    ..
                } => {
                    Self::infer_method_call_type_args_in_expr(condition, type_table);
                    Self::infer_method_call_type_args(then_block, type_table);
                    if let Some(eb) = else_block {
                        Self::infer_method_call_type_args(eb, type_table);
                    }
                }
                _ => {}
            }
        }
    }

    fn infer_method_call_type_args_in_expr(expr: &mut TirExpr, type_table: &TypeTable) {
        match &mut expr.kind {
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => {
                Self::infer_method_call_type_args_in_expr(receiver, type_table);
                for arg in args.iter_mut() {
                    Self::infer_method_call_type_args_in_expr(&mut arg.expr, type_table);
                }
                // If type_args is empty and the method has monomorph_info or method_info
                // suggesting it's generic, infer type_args from argument types.
                if type_args.is_empty()
                    && let Some(ref info) = func.method_info
                {
                    // Check if method_type_args is empty (meaning inferred type args)
                    if info.method_type_args.is_empty() {
                        // Try to infer from first non-self arg's inner type.
                        // For element<T: Serialize>(&mut self, value: &T), the first
                        // arg is &T, so T is the inner type of the first arg.
                        if let Some(first_arg) = args.first() {
                            let mut arg_type = first_arg.expr.type_id;
                            // Unwrap references
                            while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                type_table.get(arg_type).clone()
                            {
                                arg_type = t;
                            }
                            // Unwrap Box<T> (auto-boxed primitive ref)
                            if let ResolvedType::GenericInstance {
                                name,
                                type_args: ta,
                                ..
                            } = type_table.get(arg_type)
                                && name == "Box"
                                && ta.len() == 1
                            {
                                arg_type = ta[0];
                            }
                            // Only set if arg_type is concrete (not a type param)
                            // AND not already present in monomorph_info.type_args.
                            // If it's already there, it's an impl type arg (e.g.,
                            // Array<String>::append where T=String comes from the
                            // impl, not a method-level type param). Adding it again
                            // would cause double-counting in instantiation.
                            let is_concrete = !matches!(
                                type_table.get(arg_type),
                                ResolvedType::TypeParam { .. }
                                    | ResolvedType::TypePack { .. }
                                    | ResolvedType::Unknown
                            );
                            // Check if the inferred type is already an impl type arg
                            // of the receiver's struct. If so, it's not a method-level
                            // type arg and should not be added (to avoid double-counting
                            // in instantiation). e.g., Array<String>::append infers
                            // String from the arg, but String is the impl type arg.
                            let receiver_impl_type_args = {
                                let mut base = receiver.type_id;
                                while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                    type_table.get(base).clone()
                                {
                                    base = t;
                                }
                                match type_table.get(base) {
                                    ResolvedType::GenericInstance { type_args: ta, .. } => {
                                        ta.clone()
                                    }
                                    ResolvedType::BuiltinArray(elem) => vec![*elem],
                                    _ => vec![],
                                }
                            };
                            let is_impl_type_arg = receiver_impl_type_args.contains(&arg_type);
                            if is_concrete && !is_impl_type_arg {
                                type_args.push(arg_type);
                            }
                        }
                    }
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::infer_method_call_type_args(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::infer_method_call_type_args_in_expr(condition, type_table);
                Self::infer_method_call_type_args(then_branch, type_table);
                if let Some(eb) = else_branch {
                    Self::infer_method_call_type_args(eb, type_table);
                }
            }
            _ => {}
        }
    }

    /// Rewrite types in the body of a variadic for-of iteration.
    fn rewrite_variadic_binding_types(
        &self,
        block: &mut TirBlock,
        binding_local: u32,
        elem_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        for stmt in &mut block.stmts {
            self.rewrite_variadic_types_in_stmt(stmt, binding_local, elem_type, type_table);
        }
    }

    fn rewrite_variadic_types_in_stmt(
        &self,
        stmt: &mut TirStmt,
        binding_local: u32,
        elem_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.rewrite_variadic_types_in_expr(value, binding_local, elem_type, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.rewrite_variadic_types_in_expr(expr, binding_local, elem_type, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.rewrite_variadic_types_in_expr(expr, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.rewrite_variadic_types_in_expr(
                    condition,
                    binding_local,
                    elem_type,
                    type_table,
                );
                self.rewrite_variadic_binding_types(
                    then_block,
                    binding_local,
                    elem_type,
                    type_table,
                );
                if let Some(eb) = else_block {
                    self.rewrite_variadic_binding_types(eb, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.rewrite_variadic_binding_types(body, binding_local, elem_type, type_table);
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                self.rewrite_variadic_binding_types(block, binding_local, elem_type, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.rewrite_variadic_types_in_expr(
                    scrutinee,
                    binding_local,
                    elem_type,
                    type_table,
                );
                self.rewrite_variadic_binding_types(
                    then_block,
                    binding_local,
                    elem_type,
                    type_table,
                );
                if let Some(eb) = else_block {
                    self.rewrite_variadic_binding_types(eb, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.rewrite_variadic_types_in_expr(value, binding_local, elem_type, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.rewrite_variadic_types_in_expr(v, binding_local, elem_type, type_table);
                }
            }
            TirStmtKind::Continue
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::VariadicForOf { .. } => {}
        }
    }

    fn rewrite_variadic_types_in_expr(
        &self,
        expr: &mut TirExpr,
        binding_local: u32,
        elem_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } => {
                if *index == binding_local {
                    expr.type_id = elem_type;
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                self.rewrite_variadic_types_in_expr(receiver, binding_local, elem_type, type_table);
                for arg in args.iter_mut() {
                    self.rewrite_variadic_types_in_expr(
                        &mut arg.expr,
                        binding_local,
                        elem_type,
                        type_table,
                    );
                }
                // If the receiver uses the binding local, update the method's struct name
                if Self::expr_uses_local(receiver, binding_local)
                    && let Some(info) = &mut func.method_info
                {
                    let type_name = type_table.mangle_type_name(elem_type);
                    let base_name = type_table.base_type_name(elem_type);
                    *info = info.with_substituted_struct_name(&type_name, &base_name);
                    func.name = info.to_mangled_name();
                    if let Some(ms) = module_source_for_trait_impl(type_table, elem_type) {
                        func.module_source = ms;
                    }
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in args.iter_mut() {
                    self.rewrite_variadic_types_in_expr(
                        &mut arg.expr,
                        binding_local,
                        elem_type,
                        type_table,
                    );
                }
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => {
                self.rewrite_variadic_types_in_expr(inner, binding_local, elem_type, type_table);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.rewrite_variadic_binding_types(block, binding_local, elem_type, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                self.rewrite_variadic_types_in_expr(
                    condition,
                    binding_local,
                    elem_type,
                    type_table,
                );
                self.rewrite_variadic_binding_types(
                    then_branch,
                    binding_local,
                    elem_type,
                    type_table,
                );
                if let Some(eb) = else_branch {
                    self.rewrite_variadic_binding_types(eb, binding_local, elem_type, type_table);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.rewrite_variadic_types_in_expr(left, binding_local, elem_type, type_table);
                self.rewrite_variadic_types_in_expr(right, binding_local, elem_type, type_table);
            }
            TirExprKind::Assign { target, value } => {
                self.rewrite_variadic_types_in_expr(target, binding_local, elem_type, type_table);
                self.rewrite_variadic_types_in_expr(value, binding_local, elem_type, type_table);
            }
            TirExprKind::Block(block) => {
                self.rewrite_variadic_binding_types(block, binding_local, elem_type, type_table);
            }
            _ => {
                // Literals, FuncRef, GlobalVarGet, etc. — no rewriting needed
            }
        }
    }

    fn expr_uses_local(expr: &TirExpr, local_index: u32) -> bool {
        match &expr.kind {
            TirExprKind::Local { index, .. } => *index == local_index,
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Index { expr: inner, .. } => Self::expr_uses_local(inner, local_index),
            TirExprKind::StructLiteral { fields, .. } => fields
                .iter()
                .any(|f| Self::expr_uses_local(&f.value, local_index)),
            _ => false,
        }
    }

    /// Fix up local variable indices in expanded `TypePackExpansion` elements.
    ///
    /// When `[..T::method()?]` is expanded, the `?` operator's match expression
    /// creates local variables (`__qm_v`, `__qm_e`). All expanded elements share
    /// the same local indices but need different types. This allocates new locals
    /// for each element to avoid type conflicts.
    fn fixup_pack_expansion_locals(
        block: &mut TirBlock,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        for stmt in &mut block.stmts {
            Self::fixup_pack_expansion_locals_in_stmt(stmt, local_count, local_types);
        }
    }

    fn fixup_pack_expansion_locals_in_stmt(
        stmt: &mut TirStmt,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                Self::fixup_pack_expansion_locals_in_expr(expr, local_count, local_types);
            }
            TirStmtKind::Let { value, .. } => {
                Self::fixup_pack_expansion_locals_in_expr(value, local_count, local_types);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::fixup_pack_expansion_locals_in_expr(condition, local_count, local_types);
                Self::fixup_pack_expansion_locals(then_block, local_count, local_types);
                if let Some(eb) = else_block {
                    Self::fixup_pack_expansion_locals(eb, local_count, local_types);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                Self::fixup_pack_expansion_locals(body, local_count, local_types);
            }
            _ => {}
        }
    }

    fn fixup_pack_expansion_locals_in_expr(
        expr: &mut TirExpr,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        match &mut expr.kind {
            TirExprKind::TupleLiteral { elements } if elements.len() > 1 => {
                // Collect local definitions from each element.
                // If multiple elements define the same local, allocate new locals.
                let mut first_seen_locals: IndexSet<u32> = IndexSet::default();
                for (elem_idx, elem) in elements.iter_mut().enumerate() {
                    let mut locals_in_elem: Vec<u32> = Vec::new();
                    Self::collect_locals_in_expr(elem, &mut locals_in_elem);
                    if elem_idx == 0 {
                        // Update local_types for element 0's locals from expression types
                        for &local_idx in &locals_in_elem {
                            if let Some(correct_type) =
                                Self::find_local_type_in_expr(elem, local_idx)
                                && let Some(entry) = local_types.get_mut(local_idx as usize)
                            {
                                *entry = correct_type;
                            }
                        }
                        first_seen_locals.extend(locals_in_elem);
                    } else {
                        // Reallocate locals shared with previous elements;
                        // for new locals, update local_types from the expression's
                        // actual types (pattern bindings have correct per-element types
                        // but local_types may have wrong types from pack substitution).
                        let mut new_locals: Vec<u32> = Vec::new();
                        for old_idx in &locals_in_elem {
                            if first_seen_locals.contains(old_idx) {
                                let new_idx = *local_count;
                                *local_count += 1;
                                let local_type = Self::find_local_type_in_expr(elem, *old_idx)
                                    .unwrap_or(
                                        local_types
                                            .get(*old_idx as usize)
                                            .copied()
                                            .unwrap_or(TypeTable::UNIT),
                                    );
                                local_types.push(local_type);
                                Self::rewrite_local_index_in_expr(elem, *old_idx, new_idx);
                            } else {
                                if let Some(correct_type) =
                                    Self::find_local_type_in_expr(elem, *old_idx)
                                    && let Some(entry) = local_types.get_mut(*old_idx as usize)
                                {
                                    *entry = correct_type;
                                }
                                new_locals.push(*old_idx);
                            }
                        }
                        first_seen_locals.extend(new_locals);
                    }
                }
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    Self::fixup_pack_expansion_locals_in_expr(
                        &mut arg.expr,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::fixup_pack_expansion_locals(block, local_count, local_types);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::fixup_pack_expansion_locals_in_expr(scrutinee, local_count, local_types);
                for arm in arms {
                    Self::fixup_pack_expansion_locals_in_expr(
                        &mut arm.body,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            }
            | TirExprKind::FieldAccess { expr: p, .. }
            | TirExprKind::Cast { expr: p, .. }
            | TirExprKind::Unary { expr: p, .. } => {
                Self::fixup_pack_expansion_locals_in_expr(p, local_count, local_types);
            }
            TirExprKind::Binary { left, right, .. }
            | TirExprKind::Assign {
                target: left,
                value: right,
            } => {
                Self::fixup_pack_expansion_locals_in_expr(left, local_count, local_types);
                Self::fixup_pack_expansion_locals_in_expr(right, local_count, local_types);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    Self::fixup_pack_expansion_locals_in_expr(
                        &mut f.value,
                        local_count,
                        local_types,
                    );
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::fixup_pack_expansion_locals_in_expr(condition, local_count, local_types);
                Self::fixup_pack_expansion_locals(then_branch, local_count, local_types);
                if let Some(eb) = else_branch {
                    Self::fixup_pack_expansion_locals(eb, local_count, local_types);
                }
            }
            TirExprKind::Index { expr: array, index } => {
                Self::fixup_pack_expansion_locals_in_expr(array, local_count, local_types);
                Self::fixup_pack_expansion_locals_in_expr(index, local_count, local_types);
            }
            _ => {}
        }
    }

    /// Collect all local indices that are defined (via Let or pattern binding) inside an expression.
    fn collect_locals_in_expr(expr: &TirExpr, locals: &mut Vec<u32>) {
        match &expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_locals_in_expr(scrutinee, locals);
                for arm in arms {
                    Self::collect_locals_in_pattern(&arm.pattern, locals);
                    if let Some(guard) = &arm.guard {
                        Self::collect_locals_in_expr(guard, locals);
                    }
                    Self::collect_locals_in_expr(&arm.body, locals);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::collect_locals_in_block(block, locals);
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    Self::collect_locals_in_expr(&arg.expr, locals);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::collect_locals_in_expr(elem, locals);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            }
            | TirExprKind::FieldAccess { expr: p, .. }
            | TirExprKind::Cast { expr: p, .. }
            | TirExprKind::Unary { expr: p, .. } => {
                Self::collect_locals_in_expr(p, locals);
            }
            TirExprKind::Binary { left, right, .. }
            | TirExprKind::Assign {
                target: left,
                value: right,
            } => {
                Self::collect_locals_in_expr(left, locals);
                Self::collect_locals_in_expr(right, locals);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    Self::collect_locals_in_expr(&f.value, locals);
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::collect_locals_in_expr(condition, locals);
                Self::collect_locals_in_block(then_branch, locals);
                if let Some(eb) = else_branch {
                    Self::collect_locals_in_block(eb, locals);
                }
            }
            TirExprKind::Index { expr: array, index } => {
                Self::collect_locals_in_expr(array, locals);
                Self::collect_locals_in_expr(index, locals);
            }
            _ => {}
        }
    }

    fn collect_match_pattern_locals_in_stmt(stmt: &TirStmt, locals: &mut Vec<u32>) {
        match &stmt.kind {
            TirStmtKind::Expr(expr) => {
                Self::collect_match_pattern_locals_in_expr(expr, locals);
            }
            TirStmtKind::Return { value: Some(v) } => {
                Self::collect_match_pattern_locals_in_expr(v, locals);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::collect_match_pattern_locals_in_expr(condition, locals);
                for s in &then_block.stmts {
                    Self::collect_match_pattern_locals_in_stmt(s, locals);
                }
                if let Some(eb) = else_block {
                    for s in &eb.stmts {
                        Self::collect_match_pattern_locals_in_stmt(s, locals);
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_match_pattern_locals_in_expr(expr: &TirExpr, locals: &mut Vec<u32>) {
        match &expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_match_pattern_locals_in_expr(scrutinee, locals);
                for arm in arms {
                    Self::collect_locals_in_pattern(&arm.pattern, locals);
                    if let Some(guard) = &arm.guard {
                        Self::collect_match_pattern_locals_in_expr(guard, locals);
                    }
                    Self::collect_match_pattern_locals_in_expr(&arm.body, locals);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                for s in &block.stmts {
                    Self::collect_match_pattern_locals_in_stmt(s, locals);
                }
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    Self::collect_match_pattern_locals_in_expr(&arg.expr, locals);
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::collect_match_pattern_locals_in_expr(condition, locals);
                for s in &then_branch.stmts {
                    Self::collect_match_pattern_locals_in_stmt(s, locals);
                }
                if let Some(eb) = else_branch {
                    for s in &eb.stmts {
                        Self::collect_match_pattern_locals_in_stmt(s, locals);
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_locals_in_block(block: &TirBlock, locals: &mut Vec<u32>) {
        for stmt in &block.stmts {
            match &stmt.kind {
                TirStmtKind::Let { local_index, .. } => {
                    locals.push(*local_index);
                }
                TirStmtKind::Expr(expr) => Self::collect_locals_in_expr(expr, locals),
                TirStmtKind::Return { value: Some(v) } => Self::collect_locals_in_expr(v, locals),
                TirStmtKind::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    Self::collect_locals_in_expr(condition, locals);
                    Self::collect_locals_in_block(then_block, locals);
                    if let Some(eb) = else_block {
                        Self::collect_locals_in_block(eb, locals);
                    }
                }
                TirStmtKind::LabeledBlock { block, .. } | TirStmtKind::Loop { body: block } => {
                    Self::collect_locals_in_block(block, locals);
                }
                _ => {}
            }
        }
    }

    fn collect_locals_in_pattern(pattern: &TirPattern, locals: &mut Vec<u32>) {
        match pattern {
            TirPattern::Binding { local_index, .. } => {
                locals.push(*local_index);
            }
            TirPattern::Variant { bindings, .. } => {
                for b in bindings {
                    Self::collect_locals_in_pattern(b, locals);
                }
            }
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    Self::collect_locals_in_pattern(p, locals);
                }
            }
            TirPattern::Struct { fields, .. } => {
                for f in fields {
                    Self::collect_locals_in_pattern(&f.pattern, locals);
                }
            }
            _ => {}
        }
    }

    /// Find the type of a local variable definition inside an expression.
    fn find_local_type_in_expr(expr: &TirExpr, local_idx: u32) -> Option<TypeId> {
        match &expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                if let Some(t) = Self::find_local_type_in_expr(scrutinee, local_idx) {
                    return Some(t);
                }
                for arm in arms {
                    if let Some(t) = Self::find_local_type_in_pattern(&arm.pattern, local_idx) {
                        return Some(t);
                    }
                    if let Some(t) = Self::find_local_type_in_expr(&arm.body, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                Self::find_local_type_in_block(block, local_idx)
            }
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    if let Some(t) = Self::find_local_type_in_expr(&arg.expr, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    if let Some(t) = Self::find_local_type_in_expr(elem, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            }
            | TirExprKind::FieldAccess { expr: p, .. }
            | TirExprKind::Cast { expr: p, .. }
            | TirExprKind::Unary { expr: p, .. } => Self::find_local_type_in_expr(p, local_idx),
            TirExprKind::Binary { left, right, .. }
            | TirExprKind::Assign {
                target: left,
                value: right,
            } => Self::find_local_type_in_expr(left, local_idx)
                .or_else(|| Self::find_local_type_in_expr(right, local_idx)),
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    if let Some(t) = Self::find_local_type_in_expr(&f.value, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                if let Some(t) = Self::find_local_type_in_expr(condition, local_idx) {
                    return Some(t);
                }
                if let Some(t) = Self::find_local_type_in_block(then_branch, local_idx) {
                    return Some(t);
                }
                if let Some(eb) = else_branch {
                    return Self::find_local_type_in_block(eb, local_idx);
                }
                None
            }
            _ => None,
        }
    }

    fn find_local_type_in_block(block: &TirBlock, local_idx: u32) -> Option<TypeId> {
        for stmt in &block.stmts {
            match &stmt.kind {
                TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } if *local_index == local_idx => return Some(*type_id),
                TirStmtKind::Expr(expr) | TirStmtKind::Return { value: Some(expr) } => {
                    if let Some(t) = Self::find_local_type_in_expr(expr, local_idx) {
                        return Some(t);
                    }
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    if let Some(t) = Self::find_local_type_in_block(then_block, local_idx) {
                        return Some(t);
                    }
                    if let Some(eb) = else_block
                        && let Some(t) = Self::find_local_type_in_block(eb, local_idx)
                    {
                        return Some(t);
                    }
                }
                TirStmtKind::LabeledBlock { block, .. } | TirStmtKind::Loop { body: block } => {
                    if let Some(t) = Self::find_local_type_in_block(block, local_idx) {
                        return Some(t);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn find_local_type_in_pattern(pattern: &TirPattern, local_idx: u32) -> Option<TypeId> {
        match pattern {
            TirPattern::Binding {
                local_index,
                type_id,
                ..
            } if *local_index == local_idx => Some(*type_id),
            TirPattern::Variant { bindings, .. } => {
                for b in bindings {
                    if let Some(t) = Self::find_local_type_in_pattern(b, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    if let Some(t) = Self::find_local_type_in_pattern(p, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            TirPattern::Struct { fields, .. } => {
                for f in fields {
                    if let Some(t) = Self::find_local_type_in_pattern(&f.pattern, local_idx) {
                        return Some(t);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn rewrite_local_index_in_pattern(pattern: &mut TirPattern, old_idx: u32, new_idx: u32) {
        match pattern {
            TirPattern::Binding { local_index, .. } => {
                if *local_index == old_idx {
                    *local_index = new_idx;
                }
            }
            TirPattern::Variant { bindings, .. } => {
                for b in bindings {
                    Self::rewrite_local_index_in_pattern(b, old_idx, new_idx);
                }
            }
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    Self::rewrite_local_index_in_pattern(p, old_idx, new_idx);
                }
            }
            TirPattern::Struct { fields, .. } => {
                for f in fields {
                    Self::rewrite_local_index_in_pattern(&mut f.pattern, old_idx, new_idx);
                }
            }
            _ => {}
        }
    }

    /// Collect all unique `type_ids` from Return statement values in an expression.
    /// These are the GENERIC types before any substitution, used to compute
    /// wrong/correct type pairs for pack expansion fixup.
    fn collect_return_value_types(expr: &TirExpr) -> IndexSet<TypeId> {
        let mut types = IndexSet::default();
        Self::collect_return_value_types_in_expr(expr, &mut types);
        types
    }

    fn collect_return_value_types_in_expr(expr: &TirExpr, types: &mut IndexSet<TypeId>) {
        match &expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_return_value_types_in_expr(scrutinee, types);
                for arm in arms {
                    Self::collect_return_value_types_in_expr(&arm.body, types);
                }
            }
            TirExprKind::Block(block) => {
                for stmt in &block.stmts {
                    match &stmt.kind {
                        TirStmtKind::Return { value: Some(v) } => {
                            Self::collect_return_value_type_ids(v, types);
                        }
                        TirStmtKind::Expr(e) => {
                            Self::collect_return_value_types_in_expr(e, types);
                        }
                        TirStmtKind::If {
                            then_block,
                            else_block,
                            ..
                        } => {
                            for s in &then_block.stmts {
                                if let TirStmtKind::Return { value: Some(v) } = &s.kind {
                                    Self::collect_return_value_type_ids(v, types);
                                }
                            }
                            if let Some(eb) = else_block {
                                for s in &eb.stmts {
                                    if let TirStmtKind::Return { value: Some(v) } = &s.kind {
                                        Self::collect_return_value_type_ids(v, types);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn collect_return_value_type_ids(expr: &TirExpr, types: &mut IndexSet<TypeId>) {
        types.insert(expr.type_id);
        if let TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } = &expr.kind
        {
            types.insert(*variant_type);
            if let Some(p) = payload {
                Self::collect_return_value_type_ids(p, types);
            }
        }
    }

    /// Fix up Return statements inside a type pack expansion element.
    ///
    /// The per-element substitution turns `[..T]` into `[elem_type]` (a single-element
    /// tuple) in Return statements, but it should be `concrete_pack` (the full tuple).
    /// This walks the expression tree and replaces the wrong type in Return values.
    fn fixup_return_types_in_expr(
        expr: &mut TirExpr,
        wrong_type: TypeId,
        correct_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        match &mut expr.kind {
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::fixup_return_types_in_expr(scrutinee, wrong_type, correct_type, type_table);
                for arm in arms {
                    Self::fixup_return_types_in_expr(
                        &mut arm.body,
                        wrong_type,
                        correct_type,
                        type_table,
                    );
                }
            }
            TirExprKind::Block(block) => {
                Self::fixup_return_types_in_block(block, wrong_type, correct_type, type_table);
            }
            _ => {}
        }
    }

    fn fixup_return_types_in_block(
        block: &mut TirBlock,
        wrong_type: TypeId,
        correct_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        for stmt in &mut block.stmts {
            match &mut stmt.kind {
                TirStmtKind::Return { value: Some(value) } => {
                    Self::fixup_return_value_type(value, wrong_type, correct_type, type_table);
                }
                TirStmtKind::Expr(expr) => {
                    Self::fixup_return_types_in_expr(expr, wrong_type, correct_type, type_table);
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    Self::fixup_return_types_in_block(
                        then_block,
                        wrong_type,
                        correct_type,
                        type_table,
                    );
                    if let Some(else_blk) = else_block {
                        Self::fixup_return_types_in_block(
                            else_blk,
                            wrong_type,
                            correct_type,
                            type_table,
                        );
                    }
                }
                TirStmtKind::LabeledBlock { block, .. } | TirStmtKind::Loop { body: block } => {
                    Self::fixup_return_types_in_block(block, wrong_type, correct_type, type_table);
                }
                _ => {}
            }
        }
    }

    /// Replace the wrong single-element tuple type with the correct full tuple type
    /// inside a Return statement's value expression (recursively in `type_ids`).
    fn fixup_return_value_type(
        expr: &mut TirExpr,
        wrong_type: TypeId,
        correct_type: TypeId,
        type_table: &mut TypeTable,
    ) {
        expr.type_id =
            Self::replace_type_in_generic(expr.type_id, wrong_type, correct_type, type_table);
        match &mut expr.kind {
            TirExprKind::VariantConstruct {
                variant_type,
                payload,
                ..
            } => {
                *variant_type = Self::replace_type_in_generic(
                    *variant_type,
                    wrong_type,
                    correct_type,
                    type_table,
                );
                if let Some(p) = payload {
                    Self::fixup_return_value_type(p, wrong_type, correct_type, type_table);
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::fixup_return_value_type(
                        &mut arg.expr,
                        wrong_type,
                        correct_type,
                        type_table,
                    );
                }
            }
            _ => {}
        }
    }

    /// Replace `old_type` with `new_type` inside generic instances.
    /// For example, Result<[i32], String> → Result<[i32,String,bool], String>
    /// when `old_type` = [i32] and `new_type` = [i32,String,bool].
    fn replace_type_in_generic(
        type_id: TypeId,
        old_type: TypeId,
        new_type: TypeId,
        type_table: &mut TypeTable,
    ) -> TypeId {
        if type_id == old_type {
            return new_type;
        }
        match type_table.get(type_id).clone() {
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| Self::replace_type_in_generic(arg, old_type, new_type, type_table))
                    .collect();
                if new_args == type_args {
                    type_id
                } else {
                    type_table.intern(ResolvedType::GenericInstance {
                        name,
                        module_source,
                        type_args: new_args,
                    })
                }
            }
            _ => type_id,
        }
    }

    /// Fix up local indices in expanded `VariadicForOf` blocks.
    ///
    /// After expansion, each element iteration uses the same `binding_local` index.
    /// In Wasm GC, locals have a fixed type, so each iteration needs its own local
    /// with the correct element type.
    fn fixup_variadic_for_of_locals(
        block: &mut TirBlock,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        for stmt in &mut block.stmts {
            Self::fixup_variadic_locals_in_stmt(stmt, local_count, local_types);
        }
    }

    fn fixup_variadic_locals_in_stmt(
        stmt: &mut TirStmt,
        local_count: &mut u32,
        local_types: &mut Vec<TypeId>,
    ) {
        match &mut stmt.kind {
            TirStmtKind::LabeledBlock { label, block } => {
                // Check if this is an expanded variadic for-of outer block
                if label.starts_with("__tuple_for_of_") {
                    // Find the original binding local used across iterations
                    // and allocate new locals for each iteration
                    let mut binding_local = None;
                    let mut temp_local = None;

                    // First child is the temp let, rest are iteration blocks
                    for (idx, child) in block.stmts.iter().enumerate() {
                        if idx == 0 {
                            // This is: let __tuple_N = iterable;
                            if let TirStmtKind::Let { local_index, .. } = &child.kind {
                                temp_local = Some(*local_index);
                            }
                        } else if let TirStmtKind::LabeledBlock {
                            block: iter_block, ..
                        } = &child.kind
                        {
                            // First stmt in iter block is: let item = __tuple_N.i;
                            if let Some(TirStmt {
                                kind: TirStmtKind::Let { local_index, .. },
                                ..
                            }) = iter_block.stmts.first()
                                && binding_local.is_none()
                            {
                                binding_local = Some(*local_index);
                            }
                        }
                    }

                    if let (Some(binding_idx), Some(temp_idx)) = (binding_local, temp_local) {
                        // Allocate a new local for the temp tuple (if it shares the binding idx)
                        let new_temp_local = if binding_idx == temp_idx {
                            let new_idx = *local_count;
                            *local_count += 1;
                            // Find the temp's type from the first Let stmt
                            if let TirStmtKind::Let { type_id, .. } = &block.stmts[0].kind {
                                local_types.push(*type_id);
                            }
                            Some(new_idx)
                        } else {
                            None
                        };

                        // Collect locals from match pattern bindings in the first iteration body.
                        // These include `?` expansion temporaries (__qm_v, __qm_e) that need
                        // separate locals per iteration due to differing element types.
                        // We only collect pattern binding locals (not let stmt locals or
                        // nested block locals) to avoid breaking non-? for-of bodies.
                        let first_iter_body_locals: Vec<u32> = {
                            let mut locals = Vec::new();
                            if let Some(TirStmt {
                                kind:
                                    TirStmtKind::LabeledBlock {
                                        block: first_block, ..
                                    },
                                ..
                            }) = block.stmts.get(1)
                            {
                                for s in first_block.stmts.iter().skip(1) {
                                    Self::collect_match_pattern_locals_in_stmt(s, &mut locals);
                                }
                            }
                            locals
                        };

                        // For each iteration block, allocate new locals for binding and body
                        for (iter_idx, child) in block.stmts.iter_mut().skip(1).enumerate() {
                            if let TirStmtKind::LabeledBlock {
                                block: iter_block, ..
                            } = &mut child.kind
                                && let Some(TirStmt {
                                    kind:
                                        TirStmtKind::Let {
                                            local_index,
                                            type_id,
                                            ..
                                        },
                                    ..
                                }) = iter_block.stmts.first_mut()
                            {
                                let new_idx = *local_count;
                                *local_count += 1;
                                local_types.push(*type_id);
                                let old_idx = *local_index;
                                *local_index = new_idx;
                                // Update all references to old_idx → new_idx in this block
                                for s in iter_block.stmts.iter_mut().skip(1) {
                                    Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                                }

                                // For iterations after the first, also reallocate body locals
                                // (e.g., temporaries from `?` expansion that differ in type
                                // per iteration due to different element types).
                                if iter_idx > 0 {
                                    for &body_local in &first_iter_body_locals {
                                        let new_body_idx = *local_count;
                                        *local_count += 1;
                                        let body_local_type =
                                            Self::find_local_type_in_block(iter_block, body_local)
                                                .unwrap_or(
                                                    local_types
                                                        .get(body_local as usize)
                                                        .copied()
                                                        .unwrap_or(TypeTable::UNIT),
                                                );
                                        local_types.push(body_local_type);
                                        for s in iter_block.stmts.iter_mut().skip(1) {
                                            Self::rewrite_local_index_in_stmt(
                                                s,
                                                body_local,
                                                new_body_idx,
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Fix up temp local too if needed
                        if let Some(new_temp) = new_temp_local
                            && let TirStmtKind::Let { local_index, .. } = &mut block.stmts[0].kind
                        {
                            let old = *local_index;
                            *local_index = new_temp;
                            for child in block.stmts.iter_mut().skip(1) {
                                if let TirStmtKind::LabeledBlock {
                                    block: iter_block, ..
                                } = &mut child.kind
                                {
                                    for s in &mut iter_block.stmts {
                                        Self::rewrite_local_index_in_stmt(s, old, new_temp);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Recurse into other labeled blocks
                    Self::fixup_variadic_for_of_locals(block, local_count, local_types);
                }
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                Self::fixup_variadic_for_of_locals(then_block, local_count, local_types);
                if let Some(eb) = else_block {
                    Self::fixup_variadic_for_of_locals(eb, local_count, local_types);
                }
            }
            TirStmtKind::Loop { body } => {
                Self::fixup_variadic_for_of_locals(body, local_count, local_types);
            }
            _ => {}
        }
    }

    fn rewrite_local_index_in_stmt(stmt: &mut TirStmt, old_idx: u32, new_idx: u32) {
        match &mut stmt.kind {
            TirStmtKind::Expr(expr) => Self::rewrite_local_index_in_expr(expr, old_idx, new_idx),
            TirStmtKind::Let { value, .. } => {
                Self::rewrite_local_index_in_expr(value, old_idx, new_idx);
            }
            TirStmtKind::Return { value } => {
                if let Some(e) = value {
                    Self::rewrite_local_index_in_expr(e, old_idx, new_idx);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::rewrite_local_index_in_expr(condition, old_idx, new_idx);
                for s in &mut then_block.stmts {
                    Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                }
                if let Some(eb) = else_block {
                    for s in &mut eb.stmts {
                        Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                    }
                }
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                for s in &mut block.stmts {
                    Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                }
            }
            TirStmtKind::Loop { body } => {
                for s in &mut body.stmts {
                    Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                }
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    Self::rewrite_local_index_in_expr(v, old_idx, new_idx);
                }
            }
            _ => {}
        }
    }

    fn rewrite_local_index_in_expr(expr: &mut TirExpr, old_idx: u32, new_idx: u32) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } => {
                if *index == old_idx {
                    *index = new_idx;
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::rewrite_local_index_in_expr(receiver, old_idx, new_idx);
                for arg in args {
                    Self::rewrite_local_index_in_expr(&mut arg.expr, old_idx, new_idx);
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::rewrite_local_index_in_expr(&mut arg.expr, old_idx, new_idx);
                }
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::Unary { expr: inner, .. } => {
                Self::rewrite_local_index_in_expr(inner, old_idx, new_idx);
            }
            TirExprKind::Binary { left, right, .. }
            | TirExprKind::Assign {
                target: left,
                value: right,
            } => {
                Self::rewrite_local_index_in_expr(left, old_idx, new_idx);
                Self::rewrite_local_index_in_expr(right, old_idx, new_idx);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for f in fields {
                    Self::rewrite_local_index_in_expr(&mut f.value, old_idx, new_idx);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                for s in &mut block.stmts {
                    Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                Self::rewrite_local_index_in_expr(condition, old_idx, new_idx);
                for s in &mut then_branch.stmts {
                    Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                }
                if let Some(eb) = else_branch {
                    for s in &mut eb.stmts {
                        Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                    }
                }
            }
            TirExprKind::Block(block) => {
                for s in &mut block.stmts {
                    Self::rewrite_local_index_in_stmt(s, old_idx, new_idx);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::rewrite_local_index_in_expr(scrutinee, old_idx, new_idx);
                for arm in arms {
                    Self::rewrite_local_index_in_pattern(&mut arm.pattern, old_idx, new_idx);
                    if let Some(guard) = &mut arm.guard {
                        Self::rewrite_local_index_in_expr(guard, old_idx, new_idx);
                    }
                    Self::rewrite_local_index_in_expr(&mut arm.body, old_idx, new_idx);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::rewrite_local_index_in_expr(elem, old_idx, new_idx);
                }
            }
            TirExprKind::VariantConstruct {
                payload: Some(p), ..
            } => {
                Self::rewrite_local_index_in_expr(p, old_idx, new_idx);
            }
            _ => {}
        }
    }

    fn substitute_types_in_pattern(
        &self,
        pattern: &mut TirPattern,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        match pattern {
            TirPattern::Wildcard | TirPattern::Literal(_) => {}
            TirPattern::Binding { type_id, .. } => {
                // Substitute the binding's type (e.g., type parameter T -> i32)
                *type_id = self.substitute_type(*type_id, substitution, type_table);
            }
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    self.substitute_types_in_pattern(p, substitution, type_table);
                }
            }
            TirPattern::Variant {
                enum_type,
                bindings,
                payload_type,
                ..
            } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
                // Also substitute the payload type (e.g., type parameter U -> i32)
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
                for binding in bindings {
                    self.substitute_types_in_pattern(binding, substitution, type_table);
                }
            }
            TirPattern::Enum { enum_type, .. } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
            }
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);
                for field in fields {
                    self.substitute_types_in_pattern(&mut field.pattern, substitution, type_table);
                }
            }
        }
    }

    fn substitute_types_in_expr(
        &self,
        expr: &mut TirExpr,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        // Substitute the expression's own type
        expr.type_id = self.substitute_type(expr.type_id, substitution, type_table);

        // Recurse into sub-expressions
        match &mut expr.kind {
            TirExprKind::Call {
                func: call_func,
                type_args,
                args,
                ..
            } => {
                // Substitute type args themselves
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                // For static method calls (formerly StaticCall), also update the func name
                // by delegating to the StaticCall substitution logic below via a flag.
                let is_static_method = call_func.method_info.is_some();
                if is_static_method {
                    // Inline the StaticCall substitution logic
                    if !substitution.is_empty()
                        && let Some(info) = call_func.method_info.clone()
                    {
                        let has_explicit_type_params = info.struct_name != info.base_struct_name;
                        let return_type_is_generic = matches!(
                            type_table.get(expr.type_id),
                            ResolvedType::Struct {
                                is_monomorphized: true,
                                ..
                            } | ResolvedType::GenericInstance { .. }
                                | ResolvedType::BuiltinArray(_)
                        );
                        let needs_struct_type_args = has_explicit_type_params
                            || info.is_type_param_receiver
                            || return_type_is_generic;

                        let old_func_name = call_func.name.clone();
                        let module_source = call_func.module_source.clone();

                        let existing_monomorph_type_args: Option<(Vec<TypeId>, Vec<TypeId>)> =
                            if has_explicit_type_params {
                                if let FunctionRef {
                                    monomorph_info: Some(mi),
                                    ..
                                } = &*call_func
                                {
                                    Some((mi.impl_type_args.clone(), mi.method_type_args.clone()))
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                        let mut sorted_entries: Vec<_> = substitution.iter().collect();
                        sorted_entries.sort_by_key(|(idx, _)| **idx);
                        let (type_names, sub_impl_type_args, sub_method_type_args) = if let Some(
                            (ref impl_ta, ref method_ta),
                        ) =
                            existing_monomorph_type_args
                        {
                            let sub_impl: Vec<TypeId> = impl_ta
                                .iter()
                                .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                .collect();
                            let sub_method: Vec<TypeId> = method_ta
                                .iter()
                                .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                .collect();
                            let sub_names: Vec<String> = sub_impl
                                .iter()
                                .chain(sub_method.iter())
                                .map(|&tid| type_table.mangle_type_name(tid))
                                .collect();
                            (sub_names, sub_impl, sub_method)
                        } else {
                            // No existing monomorph_info — determine split from callee's info.
                            // Use the callee's monomorph_info to understand the split.
                            let (callee_impl_count, callee_method_count) = if let FunctionRef {
                                monomorph_info: Some(mi),
                                ..
                            } = &*call_func
                            {
                                (mi.impl_type_args.len(), mi.method_type_args.len())
                            } else {
                                (0, 0)
                            };
                            let names: Vec<String> = sorted_entries
                                .iter()
                                .map(|(_, tid)| type_table.mangle_type_name(**tid))
                                .collect();
                            let tids: Vec<TypeId> =
                                sorted_entries.iter().map(|(_, tid)| **tid).collect();
                            if callee_impl_count + callee_method_count > 0
                                && tids.len() >= callee_impl_count
                            {
                                let impl_ta = tids[..callee_impl_count].to_vec();
                                let method_ta = tids[callee_impl_count..].to_vec();
                                (names, impl_ta, method_ta)
                            } else {
                                (names, tids, vec![])
                            }
                        };

                        let mut new_info = if info.is_type_param_receiver && !type_names.is_empty()
                        {
                            let base = type_table.base_type_name(*sorted_entries[0].1);
                            info.with_substituted_struct_name(&type_names[0], &base)
                        } else if needs_struct_type_args {
                            info.with_struct_type_args(&type_names)
                        } else {
                            info.clone()
                        };
                        // Substitute type params in monomorph_info method_type_args and update
                        // accordingly (e.g., R → FixedReader in a default
                        // trait method body calling Self::read::<R>(r)).
                        if let FunctionRef {
                            monomorph_info: Some(mi),
                            ..
                        } = &*call_func
                        {
                            let substituted_method_args: Vec<TypeId> = mi
                                .method_type_args
                                .iter()
                                .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                .collect();
                            let any_changed = substituted_method_args
                                .iter()
                                .zip(mi.method_type_args.iter())
                                .any(|(a, b)| a != b);
                            if any_changed {
                                new_info.method_type_args = substituted_method_args
                                    .iter()
                                    .map(|&tid| type_table.mangle_type_name(tid))
                                    .collect();
                            }
                        }
                        let new_func_name = new_info.to_mangled_name();

                        if new_func_name != old_func_name {
                            if info.is_type_param_receiver {
                                let concrete_module = self
                                    .functions
                                    .trait_method_locations
                                    .get(&new_func_name)
                                    .cloned()
                                    .or_else(|| {
                                        let concrete_type_id = sorted_entries[0].1;
                                        module_source_for_trait_impl(type_table, *concrete_type_id)
                                    });
                                let new_monomorph = if new_info.method_type_args.is_empty() {
                                    None
                                } else {
                                    let base_info = LocalMethodName::new(
                                        new_info.base_struct_name.clone(),
                                        new_info.trait_name.clone(),
                                        new_info.method_name.clone(),
                                    );
                                    let generic_name = base_info.to_mangled_name();
                                    let method_type_arg_tids: Vec<TypeId> = if let FunctionRef {
                                        monomorph_info: Some(mi),
                                        ..
                                    } = &*call_func
                                    {
                                        mi.method_type_args
                                            .iter()
                                            .map(|&tid| {
                                                self.substitute_type(tid, substitution, type_table)
                                            })
                                            .collect()
                                    } else {
                                        Vec::new()
                                    };
                                    let concrete_type_id = *sorted_entries[0].1;
                                    let impl_type_arg_tids: Vec<TypeId> = type_table
                                        .generic_type_args(concrete_type_id)
                                        .unwrap_or_default();
                                    Some(MonomorphInfo {
                                        generic_name,
                                        impl_type_args: impl_type_arg_tids,
                                        method_type_args: method_type_arg_tids,
                                        is_blanket: false,
                                    })
                                };
                                // When the call still needs monomorphization (new_monomorph is Some),
                                // use the current module source because instantiate_function will
                                // add the concrete function to this module. When monomorphization
                                // is complete (None), use the concrete module where the impl lives.
                                let resolved_module = if new_monomorph.is_some() {
                                    self.current_module_source.clone()
                                } else {
                                    concrete_module.unwrap_or_else(|| module_source.clone())
                                };
                                *call_func = FunctionRef {
                                    module_source: resolved_module,
                                    name: new_func_name,
                                    monomorph_info: new_monomorph,
                                    method_info: Some(new_info),
                                    is_cm_binding: false,
                                };
                            } else {
                                let monomorph_info = Some(MonomorphInfo {
                                    generic_name: old_func_name,
                                    impl_type_args: sub_impl_type_args,
                                    method_type_args: sub_method_type_args,
                                    is_blanket: false,
                                });
                                *call_func = FunctionRef {
                                    module_source,
                                    name: new_func_name,
                                    monomorph_info,
                                    method_info: Some(new_info),
                                    is_cm_binding: false,
                                };
                            }
                        }
                    }
                }
                for arg in args {
                    self.substitute_types_in_expr(&mut arg.expr, substitution, type_table);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                type_args,
                args,
                ..
            } => {
                self.substitute_types_in_expr(receiver, substitution, type_table);
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                for arg in args {
                    self.substitute_types_in_expr(&mut arg.expr, substitution, type_table);
                }

                // Also update the method func name if receiver type contains type params
                // e.g., Array<T>::len -> Array<i32>::len when T->i32
                if !substitution.is_empty()
                    && let Some(info) = method_func.method_info.clone()
                {
                    // Check if the struct actually needs type arg substitution.
                    // Skip for non-generic structs (e.g., String::append from template strings)
                    // that happen to appear inside a generic impl block.
                    let has_explicit_type_params = info.struct_name != info.base_struct_name;
                    let receiver_is_generic = {
                        let mut base = receiver.type_id;
                        while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                            type_table.get(base).clone()
                        {
                            base = inner;
                        }
                        matches!(
                            type_table.get(base),
                            ResolvedType::GenericInstance { .. }
                                | ResolvedType::GenericResource { .. }
                                | ResolvedType::BuiltinArray(_)
                                | ResolvedType::Struct {
                                    is_monomorphized: true,
                                    ..
                                }
                        )
                    };
                    let needs_struct_type_args = has_explicit_type_params
                        || info.is_type_param_receiver
                        || receiver_is_generic;

                    // Use structured method_info instead of parsing strings
                    let old_func_name = method_func.name.clone();
                    let module_source = method_func.module_source.clone();

                    // Build type args from substitution
                    let mut sorted_entries: Vec<_> = substitution.iter().collect();
                    sorted_entries.sort_by_key(|(idx, _)| **idx);
                    let type_names: Vec<String> = sorted_entries
                        .iter()
                        .map(|(_, tid)| type_table.mangle_type_name(**tid))
                        .collect();
                    let type_args: Vec<TypeId> =
                        sorted_entries.iter().map(|(_, tid)| **tid).collect();

                    // Apply type args to get monomorphized method info
                    // If the struct is a type param (e.g., T^Ord::cmp), substitute the struct
                    // name directly instead of adding type args.
                    // Skip for non-generic structs that don't use the enclosing type params.
                    let new_info = if info.is_type_param_receiver && !type_names.is_empty() {
                        // Use the (already-substituted) receiver type to find the concrete name.
                        // type_names[0] would be wrong when there are multiple type params
                        // (e.g. Result<T,E>: the Err(E) branch should use E's substitution,
                        // not T's).
                        let mut inner = receiver.type_id;
                        while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                            type_table.get(inner).clone()
                        {
                            inner = t;
                        }
                        let mangled = type_table.mangle_type_name_resolving_newtypes(inner);
                        let base = type_table.base_type_name(inner);
                        info.with_substituted_struct_name(&mangled, &base)
                    } else if needs_struct_type_args {
                        // Derive the struct name from the already-substituted receiver
                        // type. This is authoritative: the receiver was substituted at
                        // line 4504 and reflects the correct concrete type. Using the
                        // substitution map's type_names directly would be wrong when
                        // the map contains unrelated type params (e.g., tuple for-of
                        // element types that don't affect the struct's type args).
                        let mut recv_inner = receiver.type_id;
                        while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                            type_table.get(recv_inner).clone()
                        {
                            recv_inner = t;
                        }
                        let recv_mangled = type_table.mangle_type_name(recv_inner);
                        let recv_base = type_table.base_type_name(recv_inner);
                        info.with_substituted_struct_name(&recv_mangled, &recv_base)
                    } else {
                        info.clone()
                    };
                    let new_func_name = new_info.to_mangled_name();

                    if new_func_name != old_func_name {
                        if info.is_type_param_receiver {
                            // Type param receiver substitution redirects to a concrete method
                            // (e.g., T^Ord::cmp -> i32^Ord::cmp). The target is not a
                            // monomorphized function - it's a concrete method defined in the
                            // module where the impl block lives.
                            // First, look up the actual module from the trait method locations
                            // map. This handles user-defined trait impls on primitive types
                            // (e.g., `impl Stringify for i32` in the entry module).
                            // Fall back to type-based heuristic for built-in impls.
                            let concrete_module = self
                                .functions
                                .trait_method_locations
                                .get(&new_func_name)
                                .cloned()
                                .or_else(|| {
                                    let mut inner = receiver.type_id;
                                    while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                        type_table.get(inner).clone()
                                    {
                                        inner = t;
                                    }
                                    module_source_for_trait_impl(type_table, inner)
                                });
                            // For blanket impl methods (e.g., I^IntoIterator::into_iter where
                            // the concrete function doesn't exist directly), set is_blanket=true
                            // so the monomorphizer can queue instantiation of the template.
                            // - Direct concrete method (e.g., StrUtf8ByteIter^Iterator::next):
                            //   found in trait_method_locations → monomorph_info = None
                            // - Generic impl method (e.g., Array<u8>^IntoIterator::into_iter):
                            //   receiver has type_args → handled by receiver-based scan → None
                            // - Blanket impl method (e.g., StrUtf8ByteIter^IntoIterator::into_iter):
                            //   not in trait_method_locations, receiver has no type_args → is_blanket
                            let receiver_has_type_args = {
                                let mut inner = receiver.type_id;
                                while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                    type_table.get(inner).clone()
                                {
                                    inner = t;
                                }
                                matches!(
                                    type_table.get(inner),
                                    ResolvedType::GenericInstance {
                                        type_args: args, ..
                                    } if !args.is_empty()
                                ) || matches!(type_table.get(inner), ResolvedType::BuiltinArray(_))
                            };
                            let monomorph_info = if self
                                .functions
                                .trait_method_locations
                                .contains_key(&new_func_name)
                            {
                                // Direct concrete method found — no monomorphization needed
                                None
                            } else if receiver_has_type_args {
                                // Generic impl (e.g., Array<T>) — handled by receiver scan
                                None
                            } else {
                                // Potential blanket impl method — mark for blanket instantiation.
                                // The generic_name must match the key in generic_functions.
                                //
                                // For blanket impls with a type param receiver (e.g.,
                                // impl<I: Iterator> IntoIterator for I), the template key is
                                // "I^IntoIterator::into_iter" — use old_func_name which
                                // preserves the type param name.
                                //
                                // For methods on associated type projections (e.g.,
                                // S::SeqSerializer^SerializeSeq::element), old_func_name has
                                // the unresolved projection which doesn't match any key —
                                // use new_func_name (resolved to e.g.,
                                // NsdSeqSerializer^SerializeSeq::element).
                                let blanket_name = if old_func_name
                                    .split('^')
                                    .next()
                                    .is_some_and(|struct_part| struct_part.contains("::"))
                                {
                                    new_func_name.clone()
                                } else {
                                    old_func_name
                                };
                                Some(MonomorphInfo {
                                    generic_name: blanket_name,
                                    impl_type_args: type_args,
                                    method_type_args: vec![],
                                    is_blanket: true,
                                })
                            };
                            *method_func = FunctionRef {
                                module_source: concrete_module
                                    .unwrap_or_else(|| module_source.clone()),
                                name: new_func_name,
                                monomorph_info,
                                method_info: Some(new_info),
                                is_cm_binding: false,
                            };
                        } else {
                            // Normal monomorphization (e.g., Array<T>::len -> Array<i32>::len)
                            let (
                                existing_generic_name,
                                existing_impl_ta,
                                existing_method_ta,
                                existing_is_blanket,
                            ) = match method_func {
                                FunctionRef {
                                    monomorph_info: Some(mi),
                                    ..
                                } => (
                                    Some(mi.generic_name.clone()),
                                    Some(mi.impl_type_args.clone()),
                                    Some(mi.method_type_args.clone()),
                                    mi.is_blanket,
                                ),
                                _ => (None, None, None, false),
                            };
                            // For blanket impl calls (e.g., I^IntoIterator::into_iter),
                            // substitute the existing type_args rather than building from
                            // the enclosing substitution map.
                            let final_impl_ta = if existing_is_blanket {
                                if let Some(args) = existing_impl_ta {
                                    args.iter()
                                        .map(|&tid| {
                                            self.substitute_type(tid, substitution, type_table)
                                        })
                                        .collect()
                                } else {
                                    type_args
                                }
                            } else {
                                type_args
                            };
                            let final_method_ta = existing_method_ta.unwrap_or_default();
                            let monomorph_info = Some(MonomorphInfo {
                                generic_name: existing_generic_name.unwrap_or(old_func_name),
                                impl_type_args: final_impl_ta,
                                method_type_args: final_method_ta,
                                is_blanket: existing_is_blanket,
                            });
                            // Use the original module_source: the monomorphized method
                            // belongs to the module where the generic was defined, not the
                            // module that triggered monomorphization.
                            *method_func = FunctionRef {
                                module_source,
                                name: new_func_name,
                                monomorph_info,
                                method_info: Some(new_info),
                                is_cm_binding: false,
                            };
                        }
                    }
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::Binary { op, left, right } => {
                self.substitute_types_in_expr(left, substitution, type_table);
                self.substitute_types_in_expr(right, substitution, type_table);

                // Check if this is a comparison operator on a struct type
                // If so, desugar to trait method call
                if let Some(new_kind) =
                    self.try_desugar_comparison(expr.span, *op, left, right, type_table)
                {
                    expr.kind = new_kind;
                }
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
            TirExprKind::TupleLiteral { elements } => {
                // First pass: substitute types in all elements (skip TypePackExpansion —
                // those are expanded with per-element substitution in the second pass)
                for elem in elements.iter_mut() {
                    if !matches!(elem.kind, TirExprKind::TypePackExpansion { .. }) {
                        self.substitute_types_in_expr(elem, substitution, type_table);
                    }
                }
                // Second pass: expand TupleSpread and TypePackExpansion nodes
                let has_expansion = elements.iter().any(|e| {
                    matches!(
                        e.kind,
                        TirExprKind::TupleSpread { .. } | TirExprKind::TypePackExpansion { .. }
                    )
                });
                if has_expansion {
                    let old_elements = std::mem::take(elements);
                    for elem in old_elements {
                        if let TirExprKind::TupleSpread { ref expr } = elem.kind {
                            let inner_type = type_table.get(expr.type_id).clone();
                            if let ResolvedType::Tuple(inner_elems) = inner_type {
                                for (i, &elem_type) in inner_elems.iter().enumerate() {
                                    elements.push(TirExpr::new(
                                        TirExprKind::FieldAccess {
                                            expr: expr.clone(),
                                            field_index: i as u32,
                                            field_name: i.to_string(),
                                        },
                                        elem_type,
                                        elem.span,
                                    ));
                                }
                            } else {
                                // Single-type spread (not a tuple), keep as-is
                                elements.push(*expr.clone());
                            }
                        } else if let TirExprKind::TypePackExpansion {
                            ref call_expr,
                            pack_type_id,
                        } = elem.kind
                        {
                            // Expand type pack: for each concrete type in the pack,
                            // clone the expression and substitute with per-element types.
                            let pack_index = match type_table.get(pack_type_id) {
                                ResolvedType::TypePack { index, .. } => *index,
                                _ => 0,
                            };
                            let concrete_pack =
                                self.substitute_type(pack_type_id, substitution, type_table);
                            let pack_elems = match type_table.get(concrete_pack) {
                                ResolvedType::Tuple(elems) => elems.clone(),
                                _ => vec![concrete_pack],
                            };
                            for &elem_type in &pack_elems {
                                let mut elem_call = call_expr.as_ref().clone();
                                // Per-element substitution: pack → single element type.
                                // This correctly rewrites the static call (T::method → i32::method)
                                // and the expression's own type (TypePack → i32).
                                let mut elem_sub = substitution.clone();
                                elem_sub.insert(pack_index, elem_type);
                                self.substitute_types_in_expr(
                                    &mut elem_call,
                                    &elem_sub,
                                    type_table,
                                );
                                // Fix up Return statements: the per-element substitution
                                // incorrectly maps pack types in return positions.
                                // Compute per-element-substituted return types from the
                                // original call_expr and replace with full-sub versions.
                                for wrong_type in
                                    Self::collect_return_value_types(call_expr.as_ref())
                                {
                                    let wrong =
                                        self.substitute_type(wrong_type, &elem_sub, type_table);
                                    let correct =
                                        self.substitute_type(wrong_type, substitution, type_table);
                                    if wrong != correct {
                                        Self::fixup_return_types_in_expr(
                                            &mut elem_call,
                                            wrong,
                                            correct,
                                            type_table,
                                        );
                                    }
                                }
                                elements.push(elem_call);
                            }
                        } else {
                            elements.push(elem);
                        }
                    }
                    // Rebuild the tuple type from expanded element types
                    let new_elem_types: Vec<TypeId> = elements.iter().map(|e| e.type_id).collect();
                    expr.type_id = type_table.make_tuple(new_elem_types);
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
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner } => {
                self.substitute_types_in_expr(inner, substitution, type_table);
            }
            TirExprKind::TypePackExpansion {
                call_expr,
                pack_type_id,
            } => {
                // Don't substitute inside call_expr here — it's expanded in TupleLiteral.
                // But do substitute the pack_type_id so we can look it up later.
                *pack_type_id = self.substitute_type(*pack_type_id, substitution, type_table);
                // Note: call_expr substitution happens during TupleLiteral expansion
                // with per-element substitutions. We still need to handle it if somehow
                // encountered outside TupleLiteral context.
                self.substitute_types_in_expr(call_expr, substitution, type_table);
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
                    self.substitute_types_in_pattern(&mut arm.pattern, substitution, type_table);
                    if let Some(guard) = &mut arm.guard {
                        self.substitute_types_in_expr(guard, substitution, type_table);
                    }
                    self.substitute_types_in_expr(&mut arm.body, substitution, type_table);
                }
            }
            TirExprKind::Closure {
                params,
                body,
                captures,
                ..
            } => {
                for (_, type_id) in params {
                    *type_id = self.substitute_type(*type_id, substitution, type_table);
                }
                for cap in captures {
                    cap.type_id = self.substitute_type(cap.type_id, substitution, type_table);
                }
                self.substitute_types_in_expr(body, substitution, type_table);
            }
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                // First substitute field expressions (which will update expr.type_id)
                for field in fields {
                    self.substitute_types_in_expr(&mut field.value, substitution, type_table);
                }

                // Then substitute struct_type
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);

                // Important: expr.type_id has already been substituted (line 1605 above)
                // Use it to get the correct struct type and name
                // This handles the case where struct_type is a plain Struct but expr.type_id
                // has been properly substituted to the monomorphized version
                if expr.type_id != *struct_type {
                    *struct_type = expr.type_id;
                }

                // Update struct_name to match the (possibly monomorphized) struct_type
                match type_table.get(*struct_type) {
                    ResolvedType::Struct { name, .. } => {
                        struct_name.clone_from(name);
                    }
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        if type_args.is_empty() && !substitution.is_empty() {
                            // GenericInstance with empty type_args in a substitution context
                            // Build the name using the substitution map
                            let mut sorted_entries: Vec<_> = substitution.iter().collect();
                            sorted_entries.sort_by_key(|(idx, _)| **idx);
                            let args: Vec<String> = sorted_entries
                                .iter()
                                .map(|(_, tid)| type_table.mangle_type_name(**tid))
                                .collect();
                            *struct_name = mangle_generic_name(name, &args);
                        } else {
                            // For generic instances like Container<i32>, compute the mangled name
                            let args: Vec<String> = type_args
                                .iter()
                                .map(|arg| type_table.mangle_type_name(*arg))
                                .collect();
                            *struct_name = mangle_generic_name(name, &args);
                        }
                    }
                    _ => {}
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.substitute_types_in_expr(callee, substitution, type_table);
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::ClosureToCanonical {
                functor,
                target_fn_type,
                ..
            } => {
                self.substitute_types_in_expr(functor, substitution, type_table);
                *target_fn_type = self.substitute_type(*target_fn_type, substitution, type_table);
            }
            TirExprKind::VariantConstruct {
                variant_type,
                payload,
                ..
            } => {
                *variant_type = self.substitute_type(*variant_type, substitution, type_table);
                let original_payload_type = payload.as_ref().map(|p| p.type_id);
                if let Some(payload_expr) = payload {
                    self.substitute_types_in_expr(payload_expr, substitution, type_table);
                }
                // After substitution, if variant_type is still a bare Variant (from
                // generic library code), convert it to a GenericInstance using the
                // payload type as type arg (e.g., Option + &mut Node<String> → Option<&mut Node<String>>).
                // Only promote if the payload type was actually changed by substitution,
                // indicating the variant is generic. Non-generic variants like
                // `Shape { Circle(f64), Point }` have concrete payload types that aren't
                // affected by substitution and should NOT be promoted to GenericInstance.
                if let ResolvedType::Variant { ref name, .. } =
                    type_table.get(*variant_type).clone()
                    && let Some(payload_expr) = payload
                    && original_payload_type.is_some_and(|orig| orig != payload_expr.type_id)
                {
                    // Use make_option for Option to ensure canonical module_source
                    let new_id = if name == "Option" {
                        type_table.make_option(payload_expr.type_id)
                    } else {
                        let module_source = if let ResolvedType::Variant { module_source, .. } =
                            type_table.get(*variant_type)
                        {
                            module_source.clone()
                        } else {
                            unreachable!()
                        };
                        type_table.make_generic_instance(
                            name.clone(),
                            module_source,
                            vec![payload_expr.type_id],
                        )
                    };
                    *variant_type = new_id;
                    expr.type_id = new_id;
                }
                // Unit cases (None) will be handled by the translator's fallback
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.substitute_types_in_block(block, substitution, type_table);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirExprKind::VariantTag { expr } => {
                self.substitute_types_in_expr(expr, substitution, type_table);
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.substitute_types_in_expr(expr, substitution, type_table);
            }
            TirExprKind::VariantPayload {
                expr, payload_type, ..
            } => {
                self.substitute_types_in_expr(expr, substitution, type_table);
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.substitute_types_in_expr(scrutinee, substitution, type_table);
                for arm in arms {
                    self.substitute_types_in_block(arm, substitution, type_table);
                }
                self.substitute_types_in_block(default, substitution, type_table);
            }
            // Literals and other simple expressions
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
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.substitute_types_in_expr(inner, substitution, type_table);
                    }
                }
            }
        }
    }

    /// Rewrite function calls in all functions to use monomorphized names
    fn rewrite_function_calls_in_module(&self, module: &mut TirModule) {
        let type_table = module.type_table.borrow();
        let mut rewriter = CallRewriter {
            mono: self,
            type_table: &type_table,
        };

        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(mut body) = func.body.take() {
                rewriter.visit_block(&mut body);
                // Sync local_types with Let statement types
                Self::sync_local_types_from_lets(&body, &mut func.local_types);
                // Update all Local expression types based on local_types
                Self::update_local_expr_types(&mut body, &func.local_types);
                func.body = Some(body);
            }
        }

        // Rewrite function calls in global variable initializers
        for global in &mut module.globals {
            rewriter.visit_expr(&mut global.initializer);
        }
    }

    /// Sync `local_types` array from Let statements that may have been updated
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
                TirStmtKind::Loop { body } => {
                    Self::sync_local_types_from_lets(body, local_types);
                }
                _ => {}
            }
        }
    }

    /// Update all Local expression types based on `local_types` array
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
            TirStmtKind::Loop { body } => {
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
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::update_local_expr_types_in_expr(&mut arg.expr, local_types);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::update_local_expr_types_in_expr(receiver, local_types);
                for arg in args {
                    Self::update_local_expr_types_in_expr(&mut arg.expr, local_types);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::update_local_expr_types_in_expr(left, local_types);
                Self::update_local_expr_types_in_expr(right, local_types);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => {
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
            TirExprKind::TupleLiteral { elements } => {
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
                    if let Some(guard) = &mut arm.guard {
                        Self::update_local_expr_types_in_expr(guard, local_types);
                    }
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
            TirExprKind::ClosureToCanonical { functor, .. } => {
                Self::update_local_expr_types_in_expr(functor, local_types);
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        Self::update_local_expr_types_in_expr(inner, local_types);
                    }
                }
            }
            _ => {}
        }
    }

    fn rewrite_call_expr(&self, expr: &mut TirExpr, type_table: &TypeTable) {
        if let TirExprKind::Call {
            func, type_args, ..
        } = &mut expr.kind
        {
            let original_func_name = func.name.clone();
            let original_method_info = func.method_info.clone();
            let qualified_func_name =
                generic_function_key(func.is_method(), &func.module_source, &func.name);
            // If this is a generic call, rewrite to monomorphized name
            if !type_args.is_empty() {
                let key = InstantiationKey {
                    name: qualified_func_name,
                    impl_type_args: vec![],
                    method_type_args: type_args.clone(),
                    method_info: original_method_info.clone(),
                };
                if let Some(mangled) = self.functions.instantiated.get(&key) {
                    *func = FunctionRef {
                        module_source: self.current_module_source.clone(),
                        name: mangled.clone(),
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: original_func_name,
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args.clone(),
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
                        is_cm_binding: false,
                    };

                    if let ResolvedType::TypeParam { index, .. } = type_table.get(expr.type_id)
                        && let Some(&concrete) = key.method_type_args.get(*index as usize)
                    {
                        expr.type_id = concrete;
                    }

                    type_args.clear();
                }
            }
            // Also handle static method calls (formerly StaticCall) that need rewriting
            if let FunctionRef {
                monomorph_info: Some(monomorph),
                method_info: Some(info),
                ..
            } = func
                && (!monomorph.impl_type_args.is_empty() || !monomorph.method_type_args.is_empty())
            {
                let mut names_to_try = vec![MethodName::format_local(
                    &info.base_struct_name,
                    info.trait_name.as_deref(),
                    &info.method_name,
                )];
                if info.struct_name != info.base_struct_name {
                    names_to_try.push(MethodName::format_local(
                        &info.struct_name,
                        info.trait_name.as_deref(),
                        &info.method_name,
                    ));
                }
                for generic_method_name in names_to_try {
                    let key = InstantiationKey {
                        name: generic_method_name.clone(),
                        impl_type_args: monomorph.impl_type_args.clone(),
                        method_type_args: monomorph.method_type_args.clone(),
                        method_info: Some(info.clone()),
                    };
                    if let Some(mangled) = self.functions.instantiated.get(&key) {
                        let original_method_info = func.method_info.clone();
                        *func = FunctionRef {
                            module_source: self.current_module_source.clone(),
                            name: mangled.clone(),
                            monomorph_info: Some(MonomorphInfo {
                                generic_name: generic_method_name,
                                impl_type_args: key.impl_type_args.clone(),
                                method_type_args: key.method_type_args.clone(),
                                is_blanket: false,
                            }),
                            method_info: original_method_info,
                            is_cm_binding: false,
                        };
                        break;
                    }
                }
            }
        }
    }

    fn rewrite_method_call_expr(&self, expr: &mut TirExpr, type_table: &TypeTable) {
        let TirExprKind::MethodCall {
            receiver,
            func: method_func,
            type_args,
            ..
        } = &mut expr.kind
        else {
            return;
        };

        // Extract method name from method_info or fall back to function name
        let method_name = method_func
            .method_info
            .clone()
            .map(|info| info.method_name)
            .unwrap_or_else(|| method_func.name.clone());
        // If this is a generic method call, rewrite to monomorphized name
        if !type_args.is_empty()
            && let Some(struct_name) = self.get_struct_name_from_type(receiver.type_id, type_table)
        {
            // Try both inherent method and trait method formats
            let trait_name_opt = method_func
                .method_info
                .clone()
                .and_then(|info| info.trait_name);
            let mut names_to_try = vec![(
                MethodName::format_local(&struct_name, None, &method_name),
                None::<String>,
            )];
            if let Some(ref tn) = trait_name_opt {
                names_to_try.push((
                    MethodName::format_local(&struct_name, Some(tn), &method_name),
                    Some(tn.clone()),
                ));
            }

            let mut rewritten = false;
            for (full_method_name, _tn) in &names_to_try {
                let key = InstantiationKey {
                    name: full_method_name.clone(),
                    impl_type_args: vec![],
                    method_type_args: type_args.clone(),
                    method_info: None,
                };
                if let Some(mangled) = self.functions.instantiated.get(&key) {
                    let original_method_info = method_func.method_info.clone();
                    *method_func = FunctionRef {
                        module_source: self.current_module_source.clone(),
                        name: mangled.clone(),
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: full_method_name.clone(),
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args.clone(),
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
                        is_cm_binding: false,
                    };
                    type_args.clear();
                    rewritten = true;
                    break;
                }
            }
            // Handle "double generics": method on monomorphized generic struct
            // e.g., c.transform::<i64>() where c: Container<i32>
            // Also handles GenericInstance receivers (e.g., Option<i32>)
            if !rewritten {
                let base_info = self
                    .structs
                    .mangled_to_key
                    .get(&struct_name)
                    .map(|k| (k.name.clone(), k.impl_type_args.clone()))
                    .or_else(|| {
                        self.get_struct_info_from_type(receiver.type_id, type_table)
                            .filter(|(_, args)| !args.is_empty())
                    });
                if let Some((base_struct, impl_type_args)) = base_info {
                    // Try both inherent and trait method formats
                    let mut dg_names = vec![(
                        MethodName::format_local(&base_struct, None, &method_name),
                        None::<String>,
                    )];
                    if let Some(ref tn) = trait_name_opt {
                        dg_names.push((
                            MethodName::format_local(&base_struct, Some(tn), &method_name),
                            Some(tn.clone()),
                        ));
                    }

                    for (generic_method_name, _tn) in &dg_names {
                        let combined_key = InstantiationKey {
                            name: generic_method_name.clone(),
                            impl_type_args: impl_type_args.clone(),
                            method_type_args: type_args.clone(),
                            method_info: None,
                        };
                        if let Some(mangled) = self.functions.instantiated.get(&combined_key) {
                            let original_method_info = method_func.method_info.clone();
                            *method_func = FunctionRef {
                                module_source: self.current_module_source.clone(),
                                name: mangled.clone(),
                                monomorph_info: Some(MonomorphInfo {
                                    generic_name: generic_method_name.clone(),
                                    impl_type_args: combined_key.impl_type_args.clone(),
                                    method_type_args: combined_key.method_type_args.clone(),
                                    is_blanket: false,
                                }),
                                method_info: original_method_info,
                                is_cm_binding: false,
                            };
                            type_args.clear();

                            if let ResolvedType::TypeParam { index, .. } =
                                type_table.get(expr.type_id)
                            {
                                let impl_count = impl_type_args.len() as u32;
                                if *index < impl_count {
                                    if let Some(&concrete) =
                                        combined_key.impl_type_args.get(*index as usize)
                                    {
                                        expr.type_id = concrete;
                                    }
                                } else {
                                    let method_index = (*index - impl_count) as usize;
                                    if let Some(&concrete) =
                                        combined_key.method_type_args.get(method_index)
                                    {
                                        expr.type_id = concrete;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
        // Also handle case where type_args is empty but receiver is a GenericInstance
        // e.g., nums.index_value(0) where nums: Triple<i32>
        else if let Some((base_struct, impl_type_args)) =
            self.get_struct_info_from_type(receiver.type_id, type_table)
            && !impl_type_args.is_empty()
        {
            // Try trait method name format first (e.g., Triple^IndexValue::index_value)
            let mut possible_keys = Vec::new();
            if let Some(ref info) = method_func.method_info.clone()
                && let Some(ref trait_name) = info.trait_name
            {
                // For ref-type impls, try the ref struct name first (e.g., "&^IntoIterator::into_iter")
                if info.base_struct_name != base_struct {
                    possible_keys.push(InstantiationKey {
                        name: MethodName::format_local(
                            &info.base_struct_name,
                            Some(trait_name),
                            &method_name,
                        ),
                        impl_type_args: impl_type_args.clone(),
                        method_type_args: vec![],
                        method_info: None,
                    });
                }
                let trait_method_name =
                    MethodName::format_local(&base_struct, Some(trait_name), &method_name);
                possible_keys.push(InstantiationKey {
                    name: trait_method_name,
                    impl_type_args: impl_type_args.clone(),
                    method_type_args: vec![],
                    method_info: None,
                });
            }
            // Also try regular method format
            possible_keys.push(InstantiationKey {
                name: MethodName::format_local(&base_struct, None, &method_name),
                impl_type_args,
                method_type_args: vec![],
                method_info: None,
            });

            for key in possible_keys {
                if let Some(mangled) = self.functions.instantiated.get(&key) {
                    // Preserve original method_info
                    let original_method_info = method_func.method_info.clone();
                    *method_func = FunctionRef {
                        module_source: self.current_module_source.clone(),
                        name: mangled.clone(),
                        monomorph_info: Some(MonomorphInfo {
                            generic_name: key.name.clone(),
                            impl_type_args: key.impl_type_args.clone(),
                            method_type_args: key.method_type_args.clone(),
                            is_blanket: false,
                        }),
                        method_info: original_method_info,
                        is_cm_binding: false,
                    };
                    break;
                }
            }
        }
        // Blanket impl fallback: if the FunctionRef has monomorph_info from a
        // blanket impl, rewrite to the monomorphized function name.
        {
            let blanket_lookup = if let FunctionRef {
                monomorph_info: Some(mono),
                ..
            } = &*method_func
                && mono.is_blanket
            {
                let key = InstantiationKey {
                    name: mono.generic_name.clone(),
                    impl_type_args: mono.impl_type_args.clone(),
                    method_type_args: mono.method_type_args.clone(),
                    method_info: None,
                };
                self.functions.instantiated.get(&key).map(|mangled| {
                    (
                        mangled.clone(),
                        mono.generic_name.clone(),
                        mono.impl_type_args.clone(),
                        mono.method_type_args.clone(),
                    )
                })
            } else {
                None
            };
            if let Some((mangled, generic_name, impl_ta, method_ta)) = blanket_lookup {
                let original_method_info = method_func.method_info.clone();
                *method_func = FunctionRef {
                    module_source: self.current_module_source.clone(),
                    name: mangled,
                    monomorph_info: Some(MonomorphInfo {
                        generic_name,
                        impl_type_args: impl_ta,
                        method_type_args: method_ta,
                        is_blanket: true,
                    }),
                    method_info: original_method_info,
                    is_cm_binding: false,
                };
            }
        }
    }

    /// Try to desugar a comparison operator to a trait method call.
    ///
    /// This handles comparison operators on struct types that have `Eq` or `Ord`
    /// trait implementations. During initial resolution, generic type parameters
    /// can't be desugared because the concrete type isn't known. After type
    /// substitution during monomorphization, we can now desugar these operators.
    ///
    /// Returns `Some(new_kind)` if the binary expression should be replaced,
    /// or `None` if it should remain as is (for primitives).
    fn try_desugar_comparison(
        &self,
        span: Span,
        op: TirBinaryOp,
        left: &TirExpr,
        right: &TirExpr,
        type_table: &mut TypeTable,
    ) -> Option<TirExprKind> {
        // Get the base struct name and type args from the operand type
        let operand_type = type_table.get(left.type_id);
        let (base_struct_name, impl_type_args, type_module_source): (
            String,
            Vec<String>,
            Option<ModuleSource>,
        ) = match operand_type {
            ResolvedType::Struct {
                name,
                module_source,
                base_name,
                ..
            } => {
                let struct_name = base_name.as_deref().unwrap_or(name).to_string();
                (struct_name, vec![], Some(module_source.clone()))
            }
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), vec![], Some(module_source.clone())),
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source,
                ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                (name.clone(), args, Some(module_source.clone()))
            }
            // Primitives don't use trait-based comparison
            _ => return None,
        };

        // Handle Eq trait (== and !=)
        if matches!(op, TirBinaryOp::Eq | TirBinaryOp::NotEq) {
            let needs_negation = op == TirBinaryOp::NotEq;

            // Create receiver with reference (trait methods take &self)
            let receiver_ref_type = type_table.intern(ResolvedType::Ref(left.type_id));
            let receiver = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(left.clone()),
                },
                receiver_ref_type,
                span,
            );

            // Create argument with reference (other: &Self)
            let arg_ref_type = type_table.intern(ResolvedType::Ref(right.type_id));
            let arg_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(right.clone()),
                },
                arg_ref_type,
                span,
            );

            let method_info =
                LocalMethodName::new(base_struct_name, Some("Eq".to_string()), "eq".to_string())
                    .with_struct_type_args(&impl_type_args);
            let mangled_name = method_info.to_mangled_name();

            // Resolve the module where the trait impl lives.
            // First check trait_method_locations (populated during cross-module collection),
            // then fall back to the type's own module_source (impl is in same module as type).
            let method_module_source = self
                .functions
                .trait_method_locations
                .get(&mangled_name)
                .cloned()
                .or(type_module_source)
                .unwrap_or_else(|| self.current_module_source.clone());

            let method_call = TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef {
                    module_source: method_module_source,
                    name: mangled_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                    is_cm_binding: false,
                },
                type_args: vec![],
                args: vec![CallArg::new(arg_ref, false)],
            };

            if needs_negation {
                let bool_type =
                    type_table.intern(ResolvedType::Primitive(crate::tir::PrimitiveType::Bool));
                return Some(TirExprKind::Unary {
                    op: TirUnaryOp::Not,
                    expr: Box::new(TirExpr::new(method_call, bool_type, span)),
                });
            }
            return Some(method_call);
        }

        // Handle Ord trait (<, >, <=, >=)
        // Ord::cmp returns Ordering enum with discriminants: Less=0, Equal=1, Greater=2
        if matches!(
            op,
            TirBinaryOp::Lt | TirBinaryOp::Gt | TirBinaryOp::LtEq | TirBinaryOp::GtEq
        ) {
            // Create receiver with reference (trait methods take &self)
            let receiver_ref_type = type_table.intern(ResolvedType::Ref(left.type_id));
            let receiver = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(left.clone()),
                },
                receiver_ref_type,
                span,
            );

            // Create argument with reference (other: &Self)
            let arg_ref_type = type_table.intern(ResolvedType::Ref(right.type_id));
            let arg_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(right.clone()),
                },
                arg_ref_type,
                span,
            );

            // Get Ordering type for cmp return value
            let ordering_type_id = type_table.intern(ResolvedType::Enum {
                name: "Ordering".to_string(),
                module_source: ModuleSource::prelude(),
            });

            let method_info =
                LocalMethodName::new(base_struct_name, Some("Ord".to_string()), "cmp".to_string())
                    .with_struct_type_args(&impl_type_args);
            let mangled_name = method_info.to_mangled_name();

            // Resolve the module where the trait impl lives.
            let ord_method_module_source = self
                .functions
                .trait_method_locations
                .get(&mangled_name)
                .cloned()
                .or(type_module_source)
                .unwrap_or_else(|| self.current_module_source.clone());

            let cmp_call = TirExpr::new(
                TirExprKind::MethodCall {
                    receiver: Box::new(receiver),
                    func: FunctionRef {
                        module_source: ord_method_module_source,
                        name: mangled_name,
                        monomorph_info: None,
                        method_info: Some(method_info),
                        is_cm_binding: false,
                    },
                    type_args: vec![],
                    args: vec![CallArg::new(arg_ref, false)],
                },
                ordering_type_id,
                span,
            );

            // Determine comparison operator and Ordering variant:
            // < : cmp(a, b) == Ordering::Less
            // > : cmp(a, b) == Ordering::Greater
            // <= : cmp(a, b) != Ordering::Greater
            // >= : cmp(a, b) != Ordering::Less
            let (compare_op, case_name, case_index): (TirBinaryOp, &str, u32) = match op {
                TirBinaryOp::Lt => (TirBinaryOp::Eq, "Less", 0),
                TirBinaryOp::Gt => (TirBinaryOp::Eq, "Greater", 2),
                TirBinaryOp::LtEq => (TirBinaryOp::NotEq, "Greater", 2),
                TirBinaryOp::GtEq => (TirBinaryOp::NotEq, "Less", 0),
                _ => unreachable!(),
            };

            // Create Ordering enum value for comparison
            let ordering_variant = TirExpr::new(
                TirExprKind::EnumConstruct {
                    enum_type: ordering_type_id,
                    case_name: case_name.to_string(),
                    case_index,
                },
                ordering_type_id,
                span,
            );

            return Some(TirExprKind::Binary {
                op: compare_op,
                left: Box::new(cmp_call),
                right: Box::new(ordering_variant),
            });
        }

        None
    }

    /// Desugar comparison operators on non-primitive types in all functions.
    ///
    /// This is needed for non-generic functions (where `substitute_types_in_expr` is
    /// never called) that use `==`, `!=`, `<`, etc. on struct/variant types.
    /// Without this pass, those operators fall through to the codegen's `I32Eq` fallback,
    /// which is wrong for GC reference types (variants, structs with custom Eq).
    fn desugar_comparisons_in_module(&self, module: &mut TirModule) {
        let type_table_rc = module.type_table.clone();
        let mut desugarer = ComparisonDesugarer {
            mono: self,
            type_table: &type_table_rc,
        };
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(mut body) = func.body.take() {
                desugarer.visit_block(&mut body);
                func.body = Some(body);
            }
        }
    }
}

struct TypeRewriter<'a> {
    mono: &'a Monomorphizer,
    type_table: &'a mut TypeTable,
}

impl TypeRewriter<'_> {
    fn rewrite_type_id(&mut self, type_id: TypeId) -> TypeId {
        self.mono.rewrite_type_id(type_id, self.type_table)
    }
}

impl TirMutVisitor for TypeRewriter<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        if let TirStmtKind::Let { type_id, .. } = &mut stmt.kind {
            *type_id = self.rewrite_type_id(*type_id);
        }
        self.walk_stmt(stmt);
    }

    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        match pattern {
            TirPattern::Variant { enum_type, .. } | TirPattern::Enum { enum_type, .. } => {
                *enum_type = self.rewrite_type_id(*enum_type);
            }
            TirPattern::Struct { struct_type, .. } => {
                *struct_type = self.rewrite_type_id(*struct_type);
            }
            _ => {}
        }
        self.walk_pattern(pattern);
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        expr.type_id = self.rewrite_type_id(expr.type_id);

        if let TirExprKind::StructLiteral {
            struct_type,
            struct_name,
            ..
        } = &mut expr.kind
        {
            let original_type_id = *struct_type;
            let new_type_id = self.rewrite_type_id(original_type_id);
            *struct_type = new_type_id;
            if let Some(mangled_name) = self
                .mono
                .structs
                .type_to_mangled_name
                .get(&original_type_id)
            {
                struct_name.clone_from(mangled_name);
            } else {
                match self.type_table.get(new_type_id) {
                    ResolvedType::Struct { name, .. } => {
                        struct_name.clone_from(name);
                    }
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        let type_names: Vec<String> = type_args
                            .iter()
                            .map(|&arg| self.type_table.mangle_type_name(arg))
                            .collect();
                        *struct_name = mangle_generic_name(name, &type_names);
                    }
                    _ => {}
                }
            }
        }

        self.walk_expr(expr);
    }
}

struct CallRewriter<'a> {
    mono: &'a Monomorphizer,
    type_table: &'a TypeTable,
}

impl TirMutVisitor for CallRewriter<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        self.walk_stmt(stmt);
        // Update the Let's type_id if it was a type parameter that got substituted
        if let TirStmtKind::Let { value, type_id, .. } = &mut stmt.kind
            && self.type_table.contains_type_param(*type_id)
            && !self.type_table.contains_type_param(value.type_id)
        {
            *type_id = value.type_id;
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &expr.kind {
            TirExprKind::Call { .. } => {
                self.mono.rewrite_call_expr(expr, self.type_table);
            }
            TirExprKind::MethodCall { .. } => {
                self.mono.rewrite_method_call_expr(expr, self.type_table);
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

struct ComparisonDesugarer<'a> {
    mono: &'a Monomorphizer,
    type_table: &'a Rc<RefCell<TypeTable>>,
}

impl TirMutVisitor for ComparisonDesugarer<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // Recurse into children first, then desugar this node
        self.walk_expr(expr);

        if let TirExprKind::Binary { op, left, right } = &mut expr.kind
            && let Some(new_kind) = self.mono.try_desugar_comparison(
                expr.span,
                *op,
                left,
                right,
                &mut self.type_table.borrow_mut(),
            )
        {
            expr.kind = new_kind;
        }
    }
}

/// Determine the module where trait implementations for a concrete type are defined.
/// Used when substituting a type parameter receiver (e.g., `T^Ord::cmp` → `i32^Ord::cmp`)
/// to set the correct `module_source` so DCE can find the target function.
fn module_source_for_trait_impl(type_table: &TypeTable, type_id: TypeId) -> Option<ModuleSource> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(_) => Some(ModuleSource::primitive()),
        ResolvedType::BuiltinArray(_) => Some(ModuleSource::prelude()),
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::GenericInstance { module_source, .. }
        | ResolvedType::Enum { module_source, .. }
        | ResolvedType::Variant { module_source, .. } => Some(module_source.clone()),
        ResolvedType::Tuple(_) => Some(ModuleSource::core("serde")),
        _ => None,
    }
}
